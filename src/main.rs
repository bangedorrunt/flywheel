//! flywheel — multi-agent swarm orchestrator on the herdr backend.
//!
//! NTM-parity daily loop for opencode/pi/grok: spawn swarms, dispatch work,
//! monitor states, checkpoint progress. All tracker state lives in br/bv;
//! all coordination lives in agent-mail; herdr is the backend.

mod agentstate;
mod backend;
mod config;
mod durable;
mod fs_util;
mod herdr_shim;
mod mail_shim;
mod mcp_http;
mod output;
mod profile;
mod stages;
mod workflow_catalog;
mod crew;
mod builtin_workflows;
mod state;
mod toron_embed;
mod work;
mod cron;
mod smart_cron;

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use config::{Config, Recipe, WorkflowStep};
use herdr_shim::{
    classify_send_result, AgentInfo, AgentStatus, Client, SendVerdict, Subscription, WaitOptions,
};
use output::{die, dim, error, kind_paint, ok as paint_ok, print_json, status_paint};
use state::{log_event, EventKind};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---- stage prompts (ADR-0005 Spec 2a / P3.3) ----
//
// Resolved through the `stages` library: compiled-in defaults (the former
// STAGES const, verbatim) under semantic names + p-code aliases, overridable
// by ~/.config/flywheel/stages.toml. A `--stage` name may be a semantic name
// (`systematic-execute`) OR a p-code alias (`p6`) — both resolve to the same
// prompt (the stage_alias contract).

fn stage_prompt(name: &str) -> Result<String> {
    let lib = stages::StageLibrary::load()?;
    lib.resolve(name)
        .map(|p| p.to_string())
        .ok_or_else(|| {
            anyhow!(
                "unknown stage: {name} (known: {})",
                lib.list()
                    .iter()
                    .map(|(n, a)| format!("{n} ({a})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

// ---- CLI surface ----

#[derive(Parser)]
#[command(
    name = "flywheel",
    version = VERSION,
    about = "Multi-agent swarm orchestrator on the herdr backend",
    after_help = "Exit codes: 0 ok · 1 herdr/server error · 2 usage error.\n\
                  Machine output: --json on list/status/view; --robot-snapshot for full state.\n\
                  Feedback: https://github.com/bangedorrunt/flywheel/issues"
)]
struct Cli {
    /// Full JSON state snapshot of all flywheel swarms (scripts/agents)
    #[arg(long, global = true)]
    robot_snapshot: bool,

    /// List flywheel's machine-readable capabilities as JSON
    #[arg(long, global = true)]
    robot_capabilities: bool,

    /// Compact JSON status of all flywheel swarms
    #[arg(long, global = true)]
    robot_status: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold a project (ensure dir + .beads) without spawning
    Init { project: String },
    /// Spawn a swarm: herdr workspace + agent panes
    Spawn {
        project: String,
        #[command(flatten)]
        counts: AgentCounts,
        /// spawn recipe name from recipes.toml
        #[arg(long)]
        recipe: Option<String>,
        /// extra herdr agent kind counts: --kind claude=1 (repeatable)
        #[arg(long = "kind", value_parser = parse_kind_count)]
        kinds: Vec<(String, u32)>,
        #[arg(long)]
        label: Option<String>,
        /// do not focus the new workspace
        #[arg(long)]
        no_focus: bool,
        /// spawn agents with the peer-aware self-poll template (ADR-0001 §9.3)
        #[arg(long)]
        peer_aware: bool,
    },
    /// Extend a running swarm with more agent panes
    Add {
        project: String,
        #[command(flatten)]
        counts: AgentCounts,
        #[arg(long = "kind", value_parser = parse_kind_count)]
        kinds: Vec<(String, u32)>,
    },
    /// List flywheel swarms and their agents
    List {
        /// include non-flywheel herdr workspaces
        #[arg(long)]
        all: bool,
    },
    /// Show one swarm's agents and states
    Status { project: Option<String> },
    /// Send a prompt to one or more agents (with optional wait)
    Send {
        project: String,
        /// prompt text (or --stage for the daily-driver stages)
        msg: Option<String>,
        /// target agents: comma-separated shorts (oc,pi,gk) or "all"
        #[arg(long, default_value = "all")]
        to: String,
        /// daily-driver stage name: p1|p2|p4|p5|p6|p8
        #[arg(long)]
        stage: Option<String>,
        /// wait for the agents to settle (idle/done/blocked)
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// bypass the policy deny-pattern guard
        #[arg(long)]
        force: bool,
    },
    /// Interrupt an agent (sends Escape by default)
    Interrupt {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
        /// key names to send instead of Escape (repeatable)
        #[arg(long)]
        keys: Vec<String>,
    },
    /// Read an agent's terminal output
    View {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
        #[arg(long)]
        lines: Option<u32>,
        #[arg(long, default_value = "recent")]
        source: String,
    },
    /// Watch a swarm: stream agent state changes until --until or timeout
    Watch {
        project: String,
        #[arg(long, default_value = "all")]
        to: String,
        /// exit when agents reach this status
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// Monitor a swarm: poll every N seconds, feed queued instructions to
    /// idle/finished agents, report blockers. Exits 0 when the queue is drained
    /// and the swarm settles. Runs as a durable Resonate workflow by default —
    /// crashes resume via `flywheel loop <project>` (same invocation id).
    Loop {
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
        /// run the sync polling loop instead of the durable Resonate workflow
        #[arg(long)]
        local: bool,
        /// bead-driven mode: source work from `br ready` each poll instead of
        /// a static queue — feeds the swarm until the plan/beads are done
        #[arg(long)]
        work: bool,
        /// send prompts without landing confirmation (escape hatch; confirm
        /// is the default). Only affects the durable workflow.
        #[arg(long)]
        no_confirm: bool,
        /// confirm timeout in ms for `__send --confirm` (default: leaf default 45s)
        #[arg(long)]
        confirm_timeout_ms: Option<u64>,
    },
    /// Query the state of a durable loop invocation on the Resonate server
    LoopStatus { project: String },
    /// Zoom a pane (herdr pane.zoom)
    Zoom {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
        #[arg(long)]
        off: bool,
    },
    /// Attach to an agent's terminal (interactive; execs `herdr attach`)
    Attach {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
    },
    /// Close (kill) a swarm workspace
    Kill {
        project: String,
        /// skip the confirmation prompt
        #[arg(long)]
        force: bool,
    },
    /// Work-graph surfaces (bv/br pass-through)
    Work {
        #[command(subcommand)]
        cmd: WorkCmd,
    },
    /// Assign beads to agents via agent-mail
    Assign {
        project: String,
        #[arg(long, default_value = "all")]
        to: String,
        /// bead ids, comma-separated (bd-1,bd-2)
        #[arg(long)]
        beads: String,
        /// optional extra instruction for the agents
        #[arg(long)]
        msg: Option<String>,
    },
    /// agent-mail coordination surfaces
    Mail {
        #[command(subcommand)]
        cmd: MailCmd,
    },
    /// cross-project fan-out (contact + product; ADR-0001 §9.5)
    Xproj {
        #[command(subcommand)]
        cmd: XprojCmd,
    },
    /// git worktree swarms
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Point-in-time swarm snapshots
    Checkpoint {
        #[command(subcommand)]
        cmd: CheckpointCmd,
    },
    /// Event timeline of a swarm
    Timeline { project: String },
    /// Audit trail (the full event log)
    Audit { project: String },
    /// Saved checkpoints (alias of checkpoint list)
    History { project: String },
    /// What changed since a checkpoint
    Changes {
        project: String,
        /// checkpoint index (0 = latest); default 0
        #[arg(long, default_value = "0")]
        since: usize,
    },
    /// Re-attach to a swarm's state after a break
    Resume {
        project: String,
        /// respawn the swarm with a recipe if its workspace is gone
        #[arg(long)]
        recipe: Option<String>,
    },
    /// Aggregate counters over the event log
    Analytics { project: String },
    /// Named spawn recipes
    Recipes {
        #[command(subcommand)]
        cmd: AssetCmd,
    },
    /// Named prompt sequences (daily loop etc.)
    Workflows {
        #[command(subcommand)]
        cmd: AssetCmd,
    },
    /// Ordered multi-workflow runs
    Pipeline {
        #[command(subcommand)]
        cmd: AssetCmd,
    },
    /// Templates (alias of recipes)
    Template {
        #[command(subcommand)]
        cmd: AssetCmd,
    },
    /// Stage prompts (ADR-0005 P3.3): list semantic names + p-code aliases
    Stage {
        #[command(subcommand)]
        cmd: StageCmd,
    },
    /// Recipe + workflow in one launch
    Ensemble {
        project: String,
        #[arg(long)]
        recipe: String,
        #[arg(long)]
        workflow: String,
    },
    /// Safety posture: deps + active guards
    Safety,
    /// Active policy guards (deny patterns)
    Guards,
    /// Destructive-command policy management
    Policy {
        #[command(subcommand)]
        cmd: PolicyCmd,
    },
    /// Environment + toolchain health check
    Deps,
    /// Generate the beads-agent integration instructions for an agent kind.
    /// Omit KIND to list the available kinds. Re-run after a flywheel rebuild
    /// to refresh the generated instructions with newer workflow changes.
    Integration {
        /// agent kind: opencode|pi|grok (shorts oc|pi|gk accepted); omit to list
        kind: Option<String>,
        /// write the integration to this file instead of stdout
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Backend discovery + selection (ADR-0002 pluggable backends)
    Backend {
        #[command(subcommand)]
        cmd: BackendCmd,
    },
    /// Agent profiles (ADR-0005 Spec 1/1b) — catalog + spawn
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Squads (ADR-0005 P6) — named groups of profiles; recipes are aliases
    Squad {
        #[command(subcommand)]
        cmd: SquadCmd,
    },
    /// Runs dashboard (ADR-0005 P6.3) — state + owner + per-run cost
    Runs {
        #[command(subcommand)]
        cmd: RunsCmd,
    },
    /// Unified workflow surface (ADR-0005 Spec 5 / P3.7) — catalog, lint,
    /// run, scaffold, history, and auto-crew preview. One vocabulary, one
    /// engine: built-ins + canonical `workflows/*.yaml` + absorbed legacy
    /// `workflows.toml` / `pipelines.toml`.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
    /// Smart-cron surface (ADR-0005 Spec 3 / P4.6) — the flywheel half of
    /// cron: registered schedules + schedule-triggered workflows, pause/
    /// resume, status + history. The P4.3 cron manager drives ticks off this
    /// registry; this CLI owns the catalog + control surface.
    Cron {
        #[command(subcommand)]
        cmd: CronCmd,
    },
    /// N-round A/B debate + judge (ADR-0001 §9.1 — threaded, ack-gated)
    Debate {
        /// the proposition being debated
        proposition: String,
        /// number of debate rounds (default 3)
        #[arg(long, default_value_t = 3)]
        rounds: u32,
        /// agent names or shorts for A, B, judge (default: spawn oc/oc/gk)
        #[arg(long)]
        agent_a: Option<String>,
        #[arg(long)]
        agent_b: Option<String>,
        #[arg(long)]
        judge: Option<String>,
    },
    /// Plan → review gate → implement → verify (ADR-0001 §9.2; paper-first)
    #[command(name = "plan-review")]
    PlanReview {
        /// project path / label (workspace scope + plan dir)
        project: String,
        /// feature specification for the planner
        feature: String,
        /// max planner↔reviewer change rounds before escalate (default 2)
        #[arg(long, default_value_t = 2)]
        max_change_rounds: u32,
        /// herdr agent names (defaults: planner=oc, reviewer=pi, implementer=gk)
        #[arg(long)]
        planner: Option<String>,
        #[arg(long)]
        reviewer: Option<String>,
        #[arg(long)]
        implementer: Option<String>,
        /// optional bead id for thread plan-<id> and reservation reason
        #[arg(long)]
        bead: Option<String>,
    },
    /// Reinstall the binary from source (cargo install --path)
    Upgrade,
    /// Interactive walkthrough
    Tutorial,
    /// Local REST + SSE server (feature: serve)
    Serve {
        #[arg(long, default_value = "7337")]
        port: u16,
    },
    /// In-process toron mailbox/MCP integration (ADR-0003 Decision 3A, bd-o8j.1)
    Toron {
        #[command(subcommand)]
        cmd: ToronCmd,
    },
    /// internal: poll swarm state as JSON (durable workflow leaf)
    #[command(name = "__poll", hide = true)]
    __Poll { project: String, to: String },
    /// internal: block up to timeout collecting herdr push events (durable leaf)
    ///
    /// Forward-only `events.subscribe` — no replay. The loop's `__poll`
    /// heartbeat is the correctness floor (bd-event-subscribe-reactor).
    #[command(name = "__poll_events", hide = true)]
    __PollEvents {
        project: String,
        /// max wait in ms collecting events (default 30000)
        #[arg(long, default_value = "30000")]
        timeout_ms: u64,
        /// best-effort in-run dedup: skip events with seq <= this value
        #[arg(long)]
        after_seq: Option<u64>,
    },
    /// internal: send one prompt (durable workflow leaf)
    #[command(name = "__send", hide = true)]
    __Send {
        project: String,
        name: String,
        text: String,
        /// Wait for the prompt to land (agent reaches working|blocked).
        /// Optional timeout in ms (default 45_000 when given without a value).
        #[arg(long, num_args = 0..=1, default_missing_value = "45000")]
        confirm: Option<u64>,
        /// invocation id of the loop that delegated this prompt (last-sent
        /// registry bookkeeping)
        #[arg(long)]
        invocation: Option<String>,
        /// loop iteration at send time (last-sent registry bookkeeping)
        #[arg(long)]
        iteration: Option<u64>,
    },
    /// internal: block until a pane's output matches a pattern
    #[command(name = "__wait-output", hide = true)]
    __WaitOutput {
        project: String,
        target: String,
        pattern: String,
        /// treat the pattern as a Rust regex (herdr-side) instead of a
        /// literal substring
        #[arg(long)]
        regex: bool,
        /// max wait in ms (default: server-side default)
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    /// internal: newest last-sent record for (agent, prompt-hash)
    #[command(name = "__last-sent", hide = true)]
    __LastSent {
        project: String,
        name: String,
        hash: u64,
    },
    /// internal: notify a blocked agent via agent-mail (durable workflow leaf)
    #[command(name = "__mail-blocked", hide = true)]
    __MailBlocked {
        project: String,
        name: String,
        /// escalation tier 0..3 (bd-blocked-escalation-protocol): 0=first ping,
        /// 1=reply, 2=urgent+summarize, 3=halt
        #[arg(long, default_value_t = 0)]
        tier: u32,
    },
    /// internal: diagnose unresponsive agent (tail + health mail)
    #[command(name = "__diagnose", hide = true)]
    __Diagnose {
        project: String,
        name: String,
        #[arg(long, default_value = "unknown-stuck")]
        reason: String,
    },
    /// internal: scan orchestrator inbox for `[xproj]` swarm requests
    #[command(name = "__xproj-inbox", hide = true)]
    __XprojInbox {
        project: String,
        #[arg(long, default_value = "30")]
        limit: u32,
    },
    /// internal: spawn the swarm for a parsed `[xproj]` request (full loop branch)
    #[command(name = "__xproj-spawn", hide = true)]
    __XprojSpawn {
        project: String,
        message_id: String,
        thread_id: String,
        from: String,
        recipe: String,
        workers: u32,
    },
    /// internal: renew the orchestrator's active file reservations
    /// (durable workflow leaf, bd-mail-lease-renewal-durable)
    #[command(name = "__renew-leases", hide = true)]
    __RenewLeases {
        project: String,
        extend_seconds: u64,
    },
    /// internal: list ready beads as JSON (durable workflow leaf)
    #[command(name = "__ready", hide = true)]
    __Ready { project: String },
    /// internal: list in-progress beads as JSON (durable workflow leaf)
    #[command(name = "__inprogress", hide = true)]
    __Inprogress { project: String },
    /// internal: agent kind as text (durable workflow leaf)
    #[command(name = "__kind", hide = true)]
    __Kind { project: String, name: String },
    /// internal: recent agent output as text (durable workflow leaf)
    #[command(name = "__read", hide = true)]
    __Read { project: String, name: String },
    /// internal: replace an agent with a fresh pane instance (durable workflow leaf)
    #[command(name = "__respawn", hide = true)]
    __Respawn {
        project: String,
        name: String,
        kind: String,
        /// cg6.3: bypass the replay-idempotency guard (context_limit path —
        /// an idle-but-exhausted agent is healthy and must still respawn).
        #[arg(long)]
        force: bool,
    },
}
fn cmd_internal_last_sent(_cfg: &Config, project: &str, name: &str, hash: u64) -> Result<()> {
    let record = state::load_last_sent(project)
        .into_iter()
        .rev()
        .find(|r| r.name == name && r.text_hash == hash);
    print_json(&json!({ "record": record }));
    Ok(())
}

#[derive(Args, Default, Clone)]
struct AgentCounts {
    #[arg(long)]
    oc: Option<u32>,
    #[arg(long)]
    pi: Option<u32>,
    #[arg(long)]
    gk: Option<u32>,
}

#[derive(Subcommand)]
enum WorkCmd {
    /// bv --robot-triage: full graph triage with recommendations
    Triage,
    /// bv --robot-next: the single top pick
    Next,
    /// bv --robot-insights: graph metrics
    Insights,
    /// bv --robot-plan: parallel execution tracks
    Plan,
    /// br ready --json: ready beads
    Ready,
}

#[derive(Subcommand)]
enum MailCmd {
    /// fetch inbox (default: config identity; override with --as / FLYWHEEL_MAIL_AS / AGENT_NAME)
    Inbox {
        project: String,
        #[arg(long)]
        urgent: bool,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// send a message to registered agents
    Send {
        project: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long, default_value = "normal")]
        importance: String,
        #[arg(long)]
        ack: bool,
        /// act as this agent (required for crew peer mail; else CoralFinch/config)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// thread id (use bead id for collab threads)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
    },
    /// reserve files (advisory locks)
    Locks {
        project: String,
        #[arg(long)]
        paths: Vec<String>,
        #[arg(long, default_value = "7200")]
        ttl_seconds: u64,
        #[arg(long)]
        exclusive: bool,
        #[arg(long, default_value = "flywheel")]
        reason: String,
        /// hold locks as this agent (crew must pass --as $AGENT_NAME)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// release this identity's reservations
    Unlock {
        project: String,
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// check reservation conflicts for paths
    Conflicts {
        project: String,
        #[arg(long)]
        paths: Vec<String>,
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// list registered agents in the project
    Agents { project: String },
    /// cross-project contact (request_contact / respond / list)
    Contact {
        #[command(subcommand)]
        cmd: MailContactCmd,
    },
    /// product grouping for multi-project search/inbox
    Product {
        #[command(subcommand)]
        cmd: MailProductCmd,
    },
}

#[derive(Subcommand)]
enum ToronCmd {
    /// In-process toron mailbox health (bus + archive) as JSON
    Status,
    /// Exercise the in-process toron mailbox message surface (zero IPC)
    Mail {
        #[command(subcommand)]
        cmd: ToronMailCmd,
    },
    /// List registered agents in a project as JSON (empty array on a fresh
    /// demo mailbox — the temp SQLite bus is per-process)
    Agents {
        /// project slug
        #[arg(long)]
        project: String,
    },
    /// Serve the toron MCP endpoint from this process on a background task;
    /// blocks until Ctrl-C
    #[command(name = "start-mcp")]
    StartMcp {
        /// Streamable HTTP bind port on 127.0.0.1 (default 8000)
        #[arg(long, default_value_t = 8000)]
        port: u16,
        /// serve over stdio instead of Streamable HTTP
        #[arg(long)]
        stdio: bool,
    },
    /// Auto-wire the toron MCP server into the agent MCP configs
    /// (opencode ~/.config/opencode/opencode.json, pi ~/.pi/agent/mcp.json,
    /// grok ~/.grok/config.toml). Idempotent — re-running only updates the
    /// toron entry, never clobbers other servers.
    #[command(name = "setup-mcp")]
    SetupMcp {
        /// toron MCP server URL (default http://127.0.0.1:18080/mcp)
        #[arg(long)]
        url: Option<String>,
        /// which agents to wire: opencode,pi,grok (comma-sep; default all)
        #[arg(long, default_value = "opencode,pi,grok")]
        agents: String,
        /// also boot a toron serve --mcp-http <port> daemon (needs the toron
        /// binary on PATH and TORON_OPERATOR_KEY / TORON_KEY_PASSPHRASE set)
        #[arg(long, default_value_t = 0)]
        boot_port: u16,
        /// print what would change without touching any file
        #[arg(long)]
        dry_run: bool,
        /// also point flywheel's OWN mail backend at the same toron URL
        /// (writes `[mail.toron] url` in the flywheel config.toml, so flywheel
        /// + all agents share one mailbox)
        #[arg(long)]
        flywheel: bool,
    },
}

#[derive(Subcommand)]
enum ToronMailCmd {
    /// send a message via the in-process mailbox
    Send {
        #[arg(long)]
        project: String,
        #[arg(long)]
        from: String,
        /// comma-separated recipient names
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
    },
    /// fetch an agent's inbox from the in-process mailbox
    Inbox {
        #[arg(long)]
        project: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
enum MailContactCmd {
    /// request contact with an agent (optionally in another project)
    Request {
        project: String,
        #[arg(long)]
        to: String,
        /// target project path for cross-project contact
        #[arg(long)]
        to_project: Option<String>,
        #[arg(long, default_value = "cross-project coordination")]
        reason: String,
    },
    /// accept a pending contact request
    Approve {
        project: String,
        #[arg(long)]
        from: String,
    },
    /// deny a pending contact request
    Deny {
        project: String,
        #[arg(long)]
        from: String,
    },
    /// list contact links for the flywheel identity
    List { project: String },
    /// set contact policy: open|auto|contacts_only|block_all
    Policy {
        project: String,
        #[arg(long)]
        policy: String,
    },
}

#[derive(Subcommand)]
enum MailProductCmd {
    /// ensure a product exists (e.g. trading-research, knowledge)
    Ensure {
        #[arg(long)]
        name: String,
    },
    /// link a project into a product
    Link {
        #[arg(long)]
        product: String,
        project: String,
    },
    /// search messages across a product
    Search {
        #[arg(long)]
        product: String,
        query: String,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// product-scoped inbox for the flywheel identity
    Inbox {
        #[arg(long)]
        product: String,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
}

/// Cross-project swarm fan-out (bd-pattern-cross-project-swarm).
#[derive(Subcommand)]
enum XprojCmd {
    /// one-time contact handshake with an agent in another project
    Request {
        /// local project (sender's workspace path/label)
        project: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        to_project: String,
        #[arg(long, default_value = "cross-project swarm fan-out")]
        reason: String,
    },
    /// send a cross-project swarm request (`[xproj]` subject prefix)
    Send {
        /// local project (sender)
        project: String,
        #[arg(long)]
        to: String,
        /// absolute path or label of the receiving project
        #[arg(long)]
        to_project: String,
        /// recipe/workflow name the receiver should run
        #[arg(long)]
        recipe: String,
        /// free-form params for the receiver (strategy, asset, windows, …)
        #[arg(long, default_value = "")]
        body: String,
        /// optional product for reply search (default: trading-research)
        #[arg(long, default_value = "trading-research")]
        product: String,
        /// worker count hint for the receiver
        #[arg(long, default_value_t = 2)]
        workers: u32,
    },
    /// parse an `[xproj]` body (debug / orchestrator unit path)
    Parse {
        /// full message body or subject+body text
        text: String,
    },
}

#[derive(Subcommand)]
enum WorktreeCmd {
    List {
        project: String,
    },
    Create {
        project: String,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },
    Open {
        project: String,
        #[arg(long)]
        branch: String,
    },
    Remove {
        project: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum CheckpointCmd {
    Save {
        project: String,
        #[arg(long)]
        msg: String,
    },
    List {
        project: String,
    },
    Show {
        project: String,
    },
    Latest {
        project: String,
    },
}

#[derive(Subcommand)]
enum AssetCmd {
    List,
    Run { name: String, project: String },
}

/// `flywheel stage` subcommands (ADR-0005 P3.3): the stage library surface.
#[derive(Subcommand)]
enum StageCmd {
    /// List all stages: semantic name + p-code alias (and the override path)
    List,
}

#[derive(Subcommand)]
enum PolicyCmd {
    List,
    Add { pattern: String },
    Remove { index: usize },
    Path,
}

#[derive(Subcommand)]
enum ConfigCmd {
    List,
    Get { key: String },
    Set { key: String, value: String },
    Path,
}

/// `flywheel backend` subcommands (ADR-0002 R0.10) — discovery + selection of
/// the pluggable multiplexer/mail backends. clig.dev-disciplined: stdout=data,
/// --json on reads, exit 0/1/2.
#[derive(Subcommand)]
enum BackendCmd {
    /// List compiled-in backends (name, kind, ready|stub, active, feature)
    List,
    /// Show the active multiplexer + mail + config source + health
    Status,
    /// Persist a backend default to config.toml [backend] (env/flag still override)
    Default {
        /// multiplexer | mail
        kind: String,
        /// backend name (herdr|agentmail|superlogical|tmux|toron)
        name: String,
    },
}

/// `flywheel profile` subcommands (ADR-0005 Spec 1/1b) — the agent catalog.
/// `list`/`show` are read-only; `create`/`edit` write `profiles.toml`; `run`
/// resolves `@name` → `Backend::spawn` (host scope, operator identity) and
/// optionally invokes a workflow.
#[derive(Subcommand)]
enum ProfileCmd {
    /// List catalog profiles (compiled-in defaults + locals)
    List,
    /// Show a profile (full behavior preamble + skills)
    Show {
        /// profile name (e.g. housekeeper, or a local [profiles.foo])
        name: String,
    },
    /// Add or update a profile in ~/.config/flywheel/profiles.toml
    Create {
        /// profile name
        name: String,
        /// harness / program (pi | oc | grok | hermes | prime-agent | ...)
        #[arg(long)]
        harness: Option<String>,
        /// model id (e.g. claude-sonnet-4)
        #[arg(long)]
        model: Option<String>,
        /// role label (free-form prompt-shaping)
        #[arg(long)]
        role: Option<String>,
        /// behavior preamble (system-prompt discipline)
        #[arg(long)]
        behavior: Option<String>,
        /// short task hint
        #[arg(long)]
        task_hint: Option<String>,
        /// backend trait impl (default herdr)
        #[arg(long)]
        backend: Option<String>,
        /// default workflow name
        #[arg(long)]
        default_workflow: Option<String>,
    },
    /// Alias for `create` (same fields)
    Edit {
        /// profile name
        name: String,
        /// harness / program
        #[arg(long)]
        harness: Option<String>,
        /// model id
        #[arg(long)]
        model: Option<String>,
        /// role label
        #[arg(long)]
        role: Option<String>,
        /// behavior preamble
        #[arg(long)]
        behavior: Option<String>,
        /// short task hint
        #[arg(long)]
        task_hint: Option<String>,
        /// backend trait impl
        #[arg(long)]
        backend: Option<String>,
        /// default workflow name
        #[arg(long)]
        default_workflow: Option<String>,
    },
    /// Resolve @name (host scope) and spawn the agent via the configured
    /// backend. Optionally invoke a workflow after spawn.
    Run {
        /// profile name (e.g. housekeeper)
        name: String,
        /// workflow to invoke after spawn (resolves via the workflow catalog)
        #[arg(long)]
        workflow: Option<String>,
        /// JSON inputs for the workflow invoke
        #[arg(long)]
        inputs: Option<String>,
    },
}

/// Squad commands (ADR-0005 P6): a squad is a named group of profile counts —
/// the generalization of the legacy `Recipe`. `squad spawn <recipe>` resolves
/// each member profile via `Profile::resolve` and spawns it on the backend.
#[derive(Subcommand)]
enum SquadCmd {
    /// List squads (compiled-in + from recipes.toml as aliases)
    List,
    /// Show a squad's member profiles + counts
    Show {
        /// squad / recipe name
        name: String,
    },
    /// Spawn every profile in the squad (resolves via Profile::resolve)
    Spawn {
        /// squad / recipe name (a legacy recipe maps via recipe_to_squad)
        name: String,
        /// project scope for resolve (default: host scope / operator)
        #[arg(long)]
        project: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
}

/// Runs view (ADR-0005 P6.3): the captain's daily dashboard over workflow /
/// pipeline / loop runs recorded in the project log.
#[derive(Subcommand)]
enum RunsCmd {
    /// List runs across projects: state + owner + per-run model/token cost
    List,
    /// Show one run's step trace / outputs
    Show {
        /// run id (the log entry id)
        id: String,
    },
    /// Runs parked on approval / spawn-await (pending)
    Pending,
    /// Digest: failures + parked + broken > N runs, since a cutoff
    Digest {
        /// only consider entries after this time (ISO or "N[smhd]" ago)
        #[arg(long)]
        since: Option<String>,
        /// a workflow/pipeline is "broken" after this many consecutive failures
        #[arg(long, default_value_t = 3)]
        broken_after: u32,
    },
}

/// Unified workflow commands (ADR-0005 Spec 5 / P3.7). `lint` is the
/// fail-stop gate every run must pass; `crew <name>` previews the auto-crew
/// a workflow would spawn.
#[derive(Subcommand)]
enum WorkflowCmd {
    /// List catalog + built-in workflows (name, source, trigger, crew)
    List,
    /// Show one workflow's definition
    Show {
        /// workflow name (built-in or catalog)
        name: String,
    },
    /// Lint ALL built-in + catalog workflows (the fail-stop gate). Exit 0
    /// clean, 1 on any blocking issue, 2 on a parse failure.
    Lint {
        /// lint only this workflow (name), else all
        name: Option<String>,
    },
    /// Run a workflow by name through the unified engine (lint gate first)
    Run {
        /// workflow name (built-in or catalog)
        name: String,
        /// JSON inputs for the invoke path ({{inputs.x}})
        #[arg(long)]
        inputs: Option<String>,
        /// crew strategy: auto | <squad> (default auto)
        #[arg(long)]
        crew: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Scaffold a new workflow YAML in the config dir
    New {
        /// workflow name (must be a valid identifier)
        name: String,
    },
    /// Run history for a workflow (from the state log)
    History {
        /// workflow name
        name: String,
    },
    /// Preview the auto-crew a workflow would spawn (crew: auto)
    Crew {
        /// workflow name (built-in or catalog)
        name: String,
    },
}

/// `flywheel cron` subcommands (ADR-0005 Spec 3, P4.6): the smart-cron
/// catalog + control surface. The P4.3 cron manager (Resonate durable-sleep
/// loop) drives ticks off the same `crons.toml` registry this CLI writes.
#[derive(Subcommand)]
enum CronCmd {
    /// List registered crons + schedule-triggered workflows
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Show one cron's registration + policy
    Show {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Register a new cron bound to a workflow template
    Add {
        /// cron name (identifier used by pause/resume/history/status)
        name: String,
        /// workflow template to run on each tick
        #[arg(long)]
        template: String,
        /// 5-field cron expression ("30 3 * * 0")
        #[arg(long)]
        cron: String,
        /// owning profile (Spec 3: the fixer's identity)
        #[arg(long)]
        owner: String,
        /// signing scope: host = operator identity, project = scoped agent
        #[arg(long, value_parser = ["host", "project"])]
        scope: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Pause a cron (entry preserved; nothing fires)
    Pause {
        /// cron name
        name: String,
    },
    /// Resume a paused cron
    Resume {
        /// cron name
        name: String,
    },
    /// Status of every cron: paused | due | running | succeeded | failed
    Status {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Run history for a cron (state-log scan + failure beads)
    History {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Start (or resume) the durable cron loops over the current registry
    /// (P4.3 cron manager). Health-gate probes + `on_failure: bead` are live.
    /// Blocks; Ctrl-C to stop.
    Start,
}

fn parse_kind_count(s: &str) -> Result<(String, u32)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("expected KIND=COUNT, got {s}"))?;
    Ok((k.to_string(), v.parse()?))
}

// ---- swarm helpers ----

fn project_dir(cfg: &Config, project: &str) -> PathBuf {
    let p = PathBuf::from(project);
    if p.is_absolute() {
        p
    } else {
        cfg.workspace_root().join(project)
    }
}

/// Fallible UTF-8 conversion of a filesystem path (193.1). Paths used as
/// herdr cwd args and agent-mail keys must ROUND-TRIP — lossy conversion
/// would silently corrupt them. The wire protocols are JSON (UTF-8), so a
/// non-UTF-8 path is a clear error, never a panic and never silent
/// corruption.
fn utf8_path(p: &Path, what: &str) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{what} is not valid UTF-8: {p:?}"))
}

/// `project_dir` as a UTF-8 string (193.1): the project path is a real path
/// (herdr cwd, agent-mail key) — propagate non-UTF-8 as an error.
fn project_dir_str(cfg: &Config, project: &str) -> Result<String> {
    utf8_path(&project_dir(cfg, project), &format!("project path for {project:?}"))
}

fn swarm_label(project: &str) -> String {
    let base = project.rsplit('/').next().unwrap_or(project).to_string();
    format!("flywheel-{base}")
}

fn agent_name(project: &str, short: &str) -> String {
    agent_name_n(project, short, 1)
}

/// Name the `ordinal`-th (1-based) agent of a kind in a swarm: the first keeps
/// the bare short (`flywheel-<base>-oc`), later ones are digit-suffixed
/// (`flywheel-<base>-oc2`, `-oc3`). Numbered suffixes keep `--to oc` working
/// (prefix match) and stop duplicate agent names on `add` / recipe+flag mixes.
fn agent_name_n(project: &str, short: &str, ordinal: u32) -> String {
    let base = project_base(project);
    if ordinal <= 1 {
        format!("flywheel-{base}-{short}")
    } else {
        format!("flywheel-{base}-{short}{ordinal}")
    }
}

/// Resolve a bare kind short (or any non-name value) to a concrete spawned
/// agent name for the pattern drivers (debate, plan-review). review-fix F7:
/// defaults like `oc`/`pi`/`gk` must resolve to the spawned
/// `flywheel-<base>-<short>` (the herdr agent name) so `settle_agent` finds
/// the agent and `resolve_mail_target` maps it to its registered identity.
///
/// Pass-through rule (a value that is ALREADY a concrete name):
/// - starts with `flywheel-` (a spawned herdr name) — pass through
/// - is already a valid adjective+noun identity (`agent_mail_name` is
///   idempotent on those — returns the value unchanged) — pass through
/// - contains `:` (a raw pane id) — pass through
///
/// Everything else (bare shorts like `oc`, `pi`, `gk`) expands to the first
/// ordinal of that kind.
fn resolve_agent_default(project: &str, value: &str) -> String {
    let v = value.trim();
    let already_concrete = v.starts_with("flywheel-") || v.contains(':') || agent_mail_name(v) == v;
    if v.is_empty() || already_concrete {
        v.to_string()
    } else {
        agent_name_n(project, v, 1)
    }
}

/// The ordinal of an existing agent of a kind, if its name follows the scheme.
fn agent_ordinal(name: &str, project: &str, short: &str) -> Option<u32> {
    let pfx = format!("flywheel-{}-{short}", project_base(project));
    if name == pfx {
        return Some(1);
    }
    name.strip_prefix(&pfx)?.parse().ok()
}

/// Resolve a herdr workspace for a project by label; error if missing.
fn find_workspace(c: &mut Client, project: &str) -> Result<String> {
    let label = swarm_label(project);
    let matches: Vec<herdr_shim::WorkspaceInfo> = c
        .workspace_list()?
        .into_iter()
        .filter(|ws| ws.label == label)
        .collect();
    if matches.is_empty() {
        bail!("no flywheel swarm for {project} (run: flywheel spawn {project})")
    }
    // duplicate labels (a stale workspace from a dropped agent vs the live
    // spawn — bd-nil): prefer the workspace holding managed agents; the
    // agent list is only queried when needed (fast path stays one round trip)
    if matches.len() > 1 {
        let prefix = format!("flywheel-{}-", project_base(project));
        let agents = c.agent_list()?;
        return Ok(herdr_shim::resolve_workspace_id(&matches, &agents, &prefix));
    }
    Ok(matches[0].workspace_id.clone())
}

/// Resolve target shorts (oc/pi/gk/all) to live agent infos in a workspace.
/// Names follow the scheme `flywheel-<base>-<short>[N]`, so a short matches
/// its numbered agents too (`oc` → oc, oc2, oc3) without catching unrelated
/// kinds that merely share a prefix (`ocr`).
fn resolve_agents(c: &mut Client, ws_id: &str, project: &str, to: &str) -> Result<Vec<AgentInfo>> {
    let agents = c.agent_list()?;
    let in_ws: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    if in_ws.is_empty() {
        bail!("no agents in workspace {ws_id} — is the swarm spawned?");
    }
    let avail: Vec<String> = in_ws
        .iter()
        .map(|a| a.name.clone().unwrap_or_default())
        .collect();
    let want: Vec<String> = if to == "all" {
        avail.clone()
    } else {
        to.split(',').map(|s| to_want_token(project, s)).collect()
    };
    let matched: Vec<AgentInfo> = in_ws
        .into_iter()
        .filter(|a| {
            a.name
                .as_deref()
                .map(|n| want.iter().any(|w| name_matches_want(n, w)))
                .unwrap_or(false)
        })
        .collect();
    if matched.is_empty() {
        bail!(
            "no agents match --to {to} in {project} (available: {})",
            avail.join(", ")
        );
    }
    Ok(matched)
}

/// Trait-object overloads of `find_workspace` / `resolve_agents` for the
/// durable-loop leaf handlers (`__poll` / `__send` / `__mail-blocked`), which
/// construct their backend via the factory (`backend::multiplexer_backend`)
/// per ADR-0002 §5. The one-shot CLI commands keep the `&mut Client` overloads
/// (they're herdr-specific and don't need backend-agnosticism yet — migrating
/// them is churn with no payoff until superlogical ships). The two overloads
/// share the helper bodies via macros-free duplication (small, narrow).
fn find_workspace_dyn(
    backend: &dyn backend::MultiplexerBackend,
    project: &str,
) -> Result<String> {
    let label = swarm_label(project);
    let matches: Vec<herdr_shim::WorkspaceInfo> = backend
        .workspace_list()?
        .into_iter()
        .filter(|ws| ws.label == label)
        .collect();
    if matches.is_empty() {
        bail!("no flywheel swarm for {project} (run: flywheel spawn {project})")
    }
    if matches.len() > 1 {
        let prefix = format!("flywheel-{}-", project_base(project));
        let agents = backend.agent_list()?;
        for ws in &matches {
            if agents.iter().any(|a| {
                a.workspace_id == ws.workspace_id
                    && a.name
                        .as_deref()
                        .map(|n| n.starts_with(&prefix))
                        .unwrap_or(false)
            }) {
                return Ok(ws.workspace_id.clone());
            }
        }
    }
    Ok(matches[0].workspace_id.clone())
}

fn resolve_agents_dyn(
    backend: &dyn backend::MultiplexerBackend,
    ws_id: &str,
    project: &str,
    to: &str,
) -> Result<Vec<AgentInfo>> {
    let agents = backend.agent_list()?;
    let in_ws: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    if in_ws.is_empty() {
        bail!("no agents in workspace {ws_id} — is the swarm spawned?");
    }
    let avail: Vec<String> = in_ws
        .iter()
        .map(|a| a.name.clone().unwrap_or_default())
        .collect();
    let want: Vec<String> = if to == "all" {
        avail.clone()
    } else {
        to.split(',').map(|s| to_want_token(project, s)).collect()
    };
    let matched: Vec<AgentInfo> = in_ws
        .into_iter()
        .filter(|a| {
            a.name
                .as_deref()
                .map(|n| want.iter().any(|w| name_matches_want(n, w)))
                .unwrap_or(false)
        })
        .collect();
    if matched.is_empty() {
        bail!(
            "no agents match --to {to} in {project} (available: {})",
            avail.join(", ")
        );
    }
    Ok(matched)
}

/// Normalize a single `--to` token to the concrete agent name it should
/// match against. Bare kind shorts (`oc`, `pi`, `gk`) wrap to the first
/// ordinal (`flywheel-<base>-oc`); anything already concrete passes through
/// unchanged:
///   - `flywheel-...` (a spawned herdr name, exactly what `flywheel list`
///     shows) — pass through, do NOT double-wrap
///   - contains `:` (a raw pane id) — pass through
///
/// This mirrors `resolve_agent_default`'s pass-through rule. bd-bwf: before
/// this, `flywheel send --to flywheel-toron-oc` double-wrapped to
/// `flywheel-toron-flywheel-toron-oc` and was rejected even though `list`
/// showed that exact name as available.
fn to_want_token(project: &str, token: &str) -> String {
    let t = token.trim();
    if t.is_empty() || t.starts_with("flywheel-") || t.contains(':') {
        t.to_string()
    } else {
        agent_name(project, t)
    }
}

/// Exact name, or a digit-suffixed ordinal of it (oc2 matches oc).
fn name_matches_want(name: &str, want: &str) -> bool {
    name == want
        || (name.starts_with(want)
            && name[want.len()..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false))
}

fn agent_target(a: &AgentInfo) -> &str {
    a.name.as_deref().unwrap_or(a.pane_id.as_str())
}

/// Log an event with a compact payload.
fn log(project: &str, kind: EventKind, data: Value) {
    if let Err(e) = log_event(project, kind, data) {
        eprintln!("flywheel: cannot write event log: {e:#}");
    }
}

// ---- main dispatch ----

fn main() {
    let cli = Cli::parse();

    if cli.robot_capabilities {
        print_json(&json!({
            "tool": "flywheel",
            "version": VERSION,
            "backend": format!("herdr (socket protocol v{})", herdr_shim::HDR_PROTOCOL),
            "commands": [
                "init spawn add list status send interrupt view watch zoom attach kill",
                "work assign mail worktree checkpoint timeline audit history changes resume analytics",
                "recipes workflows pipeline template ensemble safety guards policy deps config upgrade tutorial serve"
            ],
            "tracker": "br/bv (CLI, no MCP)",
            "coordination": "agent-mail (MCP over HTTP)",
            "stages": ["p1", "p2", "p4", "p5", "p6", "p8"],
            "exit_codes": { "0": "ok", "1": "herdr/server error", "2": "usage error" }
        }));
        return;
    }

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => die(&e, None),
    };

    if cli.robot_snapshot {
        let snap = robot_snapshot(&cfg);
        match snap {
            Ok(v) => {
                print_json(&v);
                return;
            }
            Err(e) => die(&e, Some("is herdr running? (flywheel deps)")),
        }
    }

    if cli.robot_status {
        let snap = robot_snapshot(&cfg);
        match snap {
            Ok(v) => {
                let compact = json!({
                    "workspaces": v["workspaces"],
                    "agents": v["agents"]
                });
                print_json(&compact);
                return;
            }
            Err(e) => die(&e, None),
        }
    }

    let result = match &cli.command {
        Some(Commands::Init { project }) => cmd_init(&cfg, project),
        Some(Commands::Spawn {
            project,
            counts,
            recipe,
            kinds,
            label,
            no_focus,
            peer_aware,
        }) => cmd_spawn(
            &cfg,
            project,
            counts,
            recipe.as_deref(),
            kinds,
            label.as_deref(),
            *no_focus,
            *peer_aware,
        ),
        Some(Commands::Add {
            project,
            counts,
            kinds,
        }) => cmd_add(&cfg, project, counts, kinds),
        Some(Commands::List { all }) => cmd_list(&cfg, *all),
        Some(Commands::Status { project }) => cmd_status(&cfg, project.as_deref()),
        Some(Commands::Send {
            project,
            msg,
            to,
            stage,
            wait,
            timeout_ms,
            force,
        }) => cmd_send(
            &cfg,
            project,
            msg.as_deref(),
            to,
            stage.as_deref(),
            *wait,
            *timeout_ms,
            *force,
        ),
        Some(Commands::Interrupt { project, to, keys }) => cmd_interrupt(&cfg, project, to, keys),
        Some(Commands::View {
            project,
            to,
            lines,
            source,
        }) => cmd_view(&cfg, project, to, *lines, source),
        Some(Commands::Watch {
            project,
            to,
            until,
            timeout_ms,
        }) => cmd_watch(&cfg, project, to, until.as_deref(), *timeout_ms),
        Some(Commands::Loop {
            project,
            interval,
            interval_active,
            queue,
            workflow,
            nudge,
            timeout_ms,
            once,
            on_blocked,
            to,
            local,
            work,
            no_confirm,
            confirm_timeout_ms,
        }) => cmd_loop(
            &cfg,
            project,
            *interval,
            *interval_active,
            queue.as_deref(),
            workflow.as_deref(),
            nudge.as_deref(),
            *timeout_ms,
            *once,
            *local,
            *work,
            on_blocked,
            to,
            *no_confirm,
            *confirm_timeout_ms,
        ),
        Some(Commands::LoopStatus { project }) => cmd_loop_status(&cfg, project),
        Some(Commands::Zoom { project, to, off }) => cmd_zoom(&cfg, project, to, *off),
        Some(Commands::Attach { project, to }) => cmd_attach(&cfg, project, to),
        Some(Commands::Kill { project, force }) => cmd_kill(&cfg, project, *force),
        Some(Commands::Work { cmd }) => cmd_work(&cfg, cmd),
        Some(Commands::Assign {
            project,
            to,
            beads,
            msg,
        }) => cmd_assign(&cfg, project, to, beads, msg.as_deref()),
        Some(Commands::Mail { cmd }) => cmd_mail(&cfg, cmd),
        Some(Commands::Xproj { cmd }) => cmd_xproj(&cfg, cmd),
        Some(Commands::Worktree { cmd }) => cmd_worktree(&cfg, cmd),
        Some(Commands::Checkpoint { cmd }) => cmd_checkpoint(&cfg, cmd),
        Some(Commands::Timeline { project }) => cmd_timeline(project),
        Some(Commands::Audit { project }) => cmd_audit(project),
        Some(Commands::History { project }) => cmd_history(project),
        Some(Commands::Changes { project, since }) => cmd_changes(&cfg, project, *since),
        Some(Commands::Resume { project, recipe }) => cmd_resume(&cfg, project, recipe.as_deref()),
        Some(Commands::Analytics { project }) => cmd_analytics(project),
        Some(Commands::Recipes { cmd }) => cmd_asset(&cfg, "recipes", cmd),
        Some(Commands::Workflows { cmd }) => cmd_asset(&cfg, "workflows", cmd),
        Some(Commands::Pipeline { cmd }) => cmd_asset(&cfg, "pipeline", cmd),
        Some(Commands::Template { cmd }) => cmd_asset(&cfg, "template", cmd),
        Some(Commands::Stage { cmd }) => cmd_stage(cmd),
        Some(Commands::Ensemble {
            project,
            recipe,
            workflow,
        }) => cmd_ensemble(&cfg, project, recipe, workflow),
        Some(Commands::Safety) => cmd_safety(&cfg),
        Some(Commands::Guards) => cmd_guards(&cfg),
        Some(Commands::Policy { cmd }) => cmd_policy(&cfg, cmd),
        Some(Commands::Deps) => cmd_deps(&cfg),
        Some(Commands::Integration { kind, out }) => {
            cmd_integration(kind.as_deref(), out.as_deref())
        }
        Some(Commands::Config { cmd }) => cmd_config(&cfg, cmd),
        Some(Commands::Backend { cmd }) => cmd_backend(&cfg, cmd),
        Some(Commands::Profile { cmd }) => cmd_profile(&cfg, cmd),
        Some(Commands::Squad { cmd }) => cmd_squad(&cfg, cmd),
        Some(Commands::Runs { cmd }) => cmd_runs(&cfg, cmd),
        Some(Commands::Workflow { cmd }) => cmd_workflow(cmd),
        Some(Commands::Cron { cmd }) => cmd_cron(cmd),
        Some(Commands::Debate {
            proposition,
            rounds,
            agent_a,
            agent_b,
            judge,
        }) => cmd_debate(
            &cfg,
            proposition,
            *rounds,
            agent_a.as_deref(),
            agent_b.as_deref(),
            judge.as_deref(),
        ),
        Some(Commands::PlanReview {
            project,
            feature,
            max_change_rounds,
            planner,
            reviewer,
            implementer,
            bead,
        }) => cmd_plan_review(
            &cfg,
            project,
            feature,
            *max_change_rounds,
            planner.as_deref(),
            reviewer.as_deref(),
            implementer.as_deref(),
            bead.as_deref(),
        ),
        Some(Commands::Upgrade) => cmd_upgrade(),
        Some(Commands::Tutorial) => cmd_tutorial(),
        Some(Commands::Serve { port }) => cmd_serve(&cfg, *port),
        Some(Commands::Toron { cmd }) => cmd_toron(&cfg, cmd),
        Some(Commands::__Poll { project, to }) => cmd_internal_poll(&cfg, project, to),
        Some(Commands::__PollEvents {
            project,
            timeout_ms,
            after_seq,
        }) => cmd_internal_poll_events(&cfg, project, *timeout_ms, *after_seq),
        Some(Commands::__Send {
            project,
            name,
            text,
            confirm,
            invocation,
            iteration,
        }) => cmd_internal_send(
            &cfg,
            project,
            name,
            text,
            *confirm,
            invocation.as_deref(),
            *iteration,
        ),
        Some(Commands::__MailBlocked {
            project,
            name,
            tier,
        }) => cmd_internal_mail_blocked(&cfg, project, name, *tier),
        Some(Commands::__Diagnose {
            project,
            name,
            reason,
        }) => cmd_internal_diagnose(&cfg, project, name, reason),
        Some(Commands::__XprojInbox { project, limit }) => {
            cmd_internal_xproj_inbox(&cfg, project, *limit)
        }
        Some(Commands::__XprojSpawn {
            project,
            message_id,
            thread_id,
            from,
            recipe,
            workers,
        }) => cmd_internal_xproj_spawn(
            &cfg,
            project,
            message_id.as_str(),
            thread_id,
            from,
            recipe,
            *workers,
        ),
        Some(Commands::__RenewLeases {
            project,
            extend_seconds,
        }) => cmd_internal_renew_leases(&cfg, project, *extend_seconds),
        Some(Commands::__WaitOutput {
            project,
            target,
            pattern,
            regex,
            timeout_ms,
        }) => cmd_internal_wait_output(&cfg, project, target, pattern, *regex, *timeout_ms),
        Some(Commands::__Ready { project }) => cmd_internal_ready(&cfg, project),
        Some(Commands::__LastSent {
            project,
            name,
            hash,
        }) => cmd_internal_last_sent(&cfg, project, name, *hash),
        Some(Commands::__Inprogress { project }) => cmd_internal_inprogress(&cfg, project),
        Some(Commands::__Kind { project, name }) => cmd_internal_kind(&cfg, project, name),
        Some(Commands::__Read { project, name }) => cmd_internal_read(&cfg, project, name),
        Some(Commands::__Respawn {
            project,
            name,
            kind,
            force,
        }) => cmd_internal_respawn(&cfg, project, name, kind, *force),
        None => {
            // no command: show concise help with examples
            print_concise_help();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        die(&e, None);
    }
}

fn print_concise_help() {
    println!(
        "flywheel — herdr swarm orchestrator (v{VERSION})\n\n\
         Examples:\n\
         \x20 flywheel spawn notetoself --oc=1 --pi=1 --gk=1\n\
         \x20 flywheel send notetoself --to all --stage p1 --wait\n\
         \x20 flywheel status notetoself\n\
         \x20 flywheel work triage\n\
         \x20 flywheel checkpoint save notetoself --msg \"before auth refactor\"\n\
         \x20 flywheel --robot-snapshot\n\n\
         Run `flywheel --help` for the full command reference."
    );
}

// ---- robot snapshot ----

fn robot_snapshot(cfg: &Config) -> Result<Value> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let workspaces = c.workspace_list()?;
    let agents = c.agent_list()?;
    let fly: Vec<Value> = workspaces
        .iter()
        .filter(|w| w.label.starts_with("flywheel-"))
        .map(|w| {
            let ws_agents: Vec<Value> = agents
                .iter()
                .filter(|a| a.workspace_id == w.workspace_id)
                .map(|a| {
                    json!({
                        "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                        "pane_id": a.pane_id, "cwd": a.cwd, "interactive_ready": a.interactive_ready
                    })
                })
                .collect();
            json!({
                "workspace_id": w.workspace_id,
                "label": w.label,
                "cwd": w.cwd,
                "pane_count": w.pane_count,
                "agents": ws_agents
            })
        })
        .collect();
    // P6.4: runs section — the same log-derived run list the `runs` CLI
    // serves, so an agent driver polls run-state from the snapshot alone.
    let runs: Vec<Value> = snapshot_runs();
    Ok(json!({
        "tool": "flywheel",
        "version": VERSION,
        "ts": state::now_iso(),
        "workspaces": fly,
        "runs": runs,
        "agents": agents.iter().map(|a| json!({
            "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
            "workspace_id": a.workspace_id, "pane_id": a.pane_id
        })).collect::<Vec<_>>()
    }))
}

/// Run list for the robot snapshot (P6.4) — workflow/pipeline/loop entries
/// from the per-project logs, newest first, with state + owner + run-id.
/// Independent of the `runs` CLI so the snapshot has no subcommand coupling.
fn snapshot_runs() -> Vec<Value> {
    use state::EventKind;
    let dir = config::state_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".jsonl") {
            continue;
        }
        let project = name.trim_end_matches(".jsonl").to_string();
        let Ok(rows) = state::read_log(&project) else {
            continue;
        };
        for e in rows {
            let run_kind = match e.kind {
                EventKind::Workflow => Some("workflow"),
                EventKind::Pipeline => Some("pipeline"),
                EventKind::Loop => Some("loop"),
                _ => None,
            };
            let Some(run_kind) = run_kind else { continue };
            let owner = ["workflow", "pipeline", "loop", "agent"]
                .iter()
                .find_map(|k| e.data.get(*k).and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            out.push(json!({
                "id": format!("{project}:{}", e.ts),
                "project": project,
                "kind": run_kind,
                "owner": owner,
                "state": e.data.get("status").and_then(Value::as_str).unwrap_or("done"),
                "ts": e.ts,
            }));
        }
    }
    out.sort_by(|a, b| {
        b.get("ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(a.get("ts").and_then(Value::as_str).unwrap_or(""))
    });
    out
}

/// Only used by the serve-gated SSE server; gate it so the lean
/// backend-only build (`--no-default-features --features backend-herdr,
/// mail-agentmail`) has no dead-code warning (bd-cargo-feature-flags).
#[cfg(feature = "serve")]
fn chrono_like_ts() -> String {
    state::now_iso()
}

// ---- init ----

fn cmd_init(cfg: &Config, project: &str) -> Result<()> {
    let dir = project_dir(cfg, project);
    if !dir.exists() {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        println!("created {}", dir.display());
    }
    if dir.join(".beads").exists() {
        println!("beads already initialized in {}", dir.display());
    } else {
        let out = Command::new("br")
            .args(["init", "--prefix", "bd"])
            .current_dir(&dir)
            .output()
            .map_err(|e| anyhow!("br not found: {e}"))?;
        if out.status.success() {
            println!("initialized beads in {}", dir.display());
        } else {
            bail!("br init failed: {}", String::from_utf8_lossy(&out.stderr));
        }
    }
    Ok(())
}

// ---- spawn ----

fn recipe_counts(
    recipe: Option<&str>,
    counts: &AgentCounts,
    kinds: &[(String, u32)],
) -> Result<Vec<(String, u32)>> {
    let mut out: Vec<(String, u32)> = Vec::new();
    if let Some(name) = recipe {
        let recipes = config::Recipes::load()?;
        let r: &Recipe = recipes
            .recipes
            .get(name)
            .ok_or_else(|| anyhow!("recipe not found: {name} (flywheel recipes list)"))?;
        if let Some(n) = r.oc {
            out.push(("oc".into(), n));
        }
        if let Some(n) = r.pi {
            out.push(("pi".into(), n));
        }
        if let Some(n) = r.gk {
            out.push(("gk".into(), n));
        }
        for (k, n) in &r.extra {
            out.push((k.clone(), *n));
        }
    }
    for (k, n) in [("oc", counts.oc), ("pi", counts.pi), ("gk", counts.gk)] {
        if let Some(n) = n && n > 0 {
            out.push((k.into(), n));
        }
    }
    for (k, n) in kinds {
        if *n > 0 {
            out.push((k.clone(), *n));
        }
    }
    if out.is_empty() {
        bail!("nothing to spawn: pass --oc/--pi/--gk counts or --recipe NAME");
    }
    Ok(out)
}

/// clap command signature — params map 1:1 to CLI flags (ADR-0002 CLI surface).
#[allow(clippy::too_many_arguments)]
fn cmd_spawn(
    cfg: &Config,
    project: &str,
    counts: &AgentCounts,
    recipe: Option<&str>,
    kinds: &[(String, u32)],
    label_override: Option<&str>,
    no_focus: bool,
    peer_aware: bool,
) -> Result<()> {
    let dir = project_dir(cfg, project);
    if !dir.exists() {
        bail!(
            "project dir missing: {} (run: flywheel init {project})",
            dir.display()
        );
    }
    let plan = recipe_counts(recipe, counts, kinds)?;
    let label = label_override
        .map(String::from)
        .unwrap_or_else(|| swarm_label(project));

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let (ws, root) = c.workspace_create(
        Some(&utf8_path(&dir, "workspace dir")?),
        Some(&label),
        !no_focus,
    )?;
    println!(
        "{}",
        if no_focus {
            format!("workspace {} created (background)", ws.workspace_id)
        } else {
            format!("workspace {} created", ws.workspace_id)
        }
    );

    let mut spawned: Vec<Value> = Vec::new();
    let mut prev = root.pane_id.clone();
    let mut first = true;
    // ordinals per kind across the whole plan (recipe + flags may repeat a kind)
    let mut ordinals: BTreeMap<String, u32> = BTreeMap::new();
    // peer-aware bootstrap prompt (bd-pattern-peer-triggered, ADR-0001 §9.3).
    // When --peer-aware, each spawned agent receives the self-poll template as
    // its initial instruction. The template is read once; per-agent
    // placeholders are substituted at the call site below.
    let peer_template: Option<String> = if peer_aware {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/peer-aware.md.tpl");
        match std::fs::read_to_string(&p) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "flywheel: cannot read peer-aware template {}: {e}",
                    p.display()
                );
                None
            }
        }
    } else {
        None
    };
    for (short, n) in &plan {
        let kind = cfg.resolve_kind(short);
        for _ in 0..*n {
            let ordinal = ordinals.entry(short.clone()).or_insert(0);
            *ordinal += 1;
            let name = agent_name_n(project, short, *ordinal);
            let peer_args: Vec<String> = peer_template
                .as_ref()
                .map(|t| {
                    vec![t
                        .replace("{name}", &name)
                        .replace("{task}", "await the swarm loop's queue or help peers")]
                })
                .unwrap_or_default();
            let pane_id = if first {
                first = false;
                prev.clone()
            } else {
                let p = herdr_shim::pane_split(
                    &mut c,
                    &prev,
                    "down",
                    Some(&utf8_path(&dir, "workspace dir")?),
                )?;
                prev = p.pane_id.clone();
                prev.clone()
            };
            let agent = start_agent_with_retry(&mut c, &name, &kind, &pane_id, &peer_args)?;
            // governed-by: ADR-0001 (mail-register-on-spawn) — register the
            // spawned agent as an agent-mail identity so it's addressable by
            // name in mail. Best-effort: mail-not-configured is NOT a spawn
            // failure (silent skip); a registration error is a warning.
            register_spawned_agent(cfg, project, &name, &kind, &agent.pane_id);
            println!(
                "  {} {}",
                kind_paint(&kind),
                dim(&format!("({})", agent.pane_id))
            );
            spawned.push(json!({
                "name": agent.name, "kind": agent.agent, "pane_id": agent.pane_id, "status": agent.agent_status.as_str()
            }));
        }
    }

    log(
        project,
        EventKind::Spawn,
        json!({
            "workspace_id": ws.workspace_id, "label": label, "agents": spawned
        }),
    );

    println!(
        "swarm ready: flywheel {} {} {}",
        kind_paint("send"),
        dim(project),
        dim("--to all --stage p1")
    );
    Ok(())
}

fn project_base(project: &str) -> String {
    project.rsplit('/').next().unwrap_or(project).to_string()
}

fn start_agent_with_retry(
    c: &mut Client,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<AgentInfo> {
    for attempt in 0..30 {
        if let Err(e) = c.agent_start(name, kind, pane_id, 45_000, args) && attempt == 0 {
            eprintln!("agent.start error: {e:#}");
        }
        // a timeout can leave the agent already booted; check the registry.
        // match name AND pane — a same-named agent on another pane is not ours.
        if let Ok(agents) = c.agent_list() {
            if let Some(a) = agents
                .iter()
                .find(|a| a.name.as_deref() == Some(name) && a.pane_id == pane_id)
            {
                return Ok(a.clone());
            }
            // pi agents frequently register NAMELESS (herdr pi integration,
            // bd-w17): accept the pane match and apply the name ourselves —
            // name-based resolution (send/loop/status) depends on it
            if let Some(a) = agents.iter().find(|a| a.pane_id == pane_id) {
                if a.name.as_deref() != Some(name) && let Err(e) = c.agent_rename(&a.pane_id, name) {
                    eprintln!("agent.rename error: {e:#}");
                }
                if let Ok(agents) = c.agent_list() && let Some(a) = agents.iter().find(|a| a.pane_id == pane_id) {
                    return Ok(a.clone());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    bail!("agent start failed for {name} on {pane_id} after 30 attempts")
}

/// Trait-object variant of [`start_agent_with_retry`] (review-fix F6): the
/// durable-loop respawn leaf routes through the backend factory, so this
/// retries via `MultiplexerBackend::agent_start` + `agent_list` instead of
/// the concrete `Client`. Same retry contract: 30 attempts, 3s sleep, accept
/// a same-pane agent even if nameless (bd-w17) and apply the name.
fn start_agent_with_retry_dyn(
    backend: &dyn backend::MultiplexerBackend,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<AgentInfo> {
    for attempt in 0..30 {
        if let Err(e) = backend.agent_start(name, kind, pane_id, args) && attempt == 0 {
            eprintln!("agent.start error: {e:#}");
        }
        if let Ok(agents) = backend.agent_list() {
            if let Some(a) = agents
                .iter()
                .find(|a| a.name.as_deref() == Some(name) && a.pane_id == pane_id)
            {
                return Ok(a.clone());
            }
            if let Some(a) = agents.iter().find(|a| a.pane_id == pane_id) {
                if a.name.as_deref() != Some(name) {
                    let _ = backend.agent_rename(&a.pane_id, Some(name));
                }
                if let Ok(agents) = backend.agent_list() && let Some(a) = agents.iter().find(|a| a.pane_id == pane_id) {
                    return Ok(a.clone());
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    bail!("agent start failed for {name} on {pane_id} after 30 attempts")
}

/// Register a spawned agent as an agent-mail identity (ADR-0001
/// mail-register-on-spawn) and bridge it to its herdr pane (ADR-0001 A4,
/// bd-mail-pane-identity-bridge): agent-mail only writes identity files for
/// tmux panes, so flywheel writes the canonical identity file for the
/// herdr pane itself — `resolve_pane_identity` can then map the pane to the
/// name. Best-effort: if mail isn't configured or the backend returns an
/// error, the spawn still succeeded — log a warning and continue. The agent
/// becomes addressable by name in mail once registered.
///
/// governed-by: ADR-0001 (bd-mail-register-on-spawn, bd-mail-pane-identity-bridge), ADR-0002 (factory)
fn register_spawned_agent(cfg: &Config, project: &str, name: &str, kind: &str, pane_id: &str) {
    let key = match project_dir(cfg, project).to_str() {
        Some(k) => k.to_string(),
        None => return,
    };
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(_) => return, // mail not configured — silent skip
    };
    if let Err(e) = backend::run_mail(mail.ensure_project(&key)) {
        eprintln!("flywheel: mail ensure_project failed for {name}: {e}");
        return;
    }
    // bd-odc fix: agent-mail's register_agent REJECTS non-adjective+noun
    // names, and flywheel spawns `flywheel-<base>-<kind>` names. Map to a
    // stable adjective+noun (agent_mail_name), keep the herdr name in
    // task_description so list_agents is the reverse lookup.
    let mail_name = agent_mail_name(name);
    let task = format!("{name} (flywheel agent for {project})");
    let profile = backend::AgentProfile {
        name: mail_name.clone(),
        program: Some(kind.to_string()),
        model: Some(kind.to_string()),
        persona: None,
        task_description: Some(task),
    };
    if let Err(e) = backend::run_mail(mail.register_agent(&key, &profile)) {
        // Best-effort: a registration failure must never fail the spawn. The
        // pane-identity bridge below still works (the identity file is the
        // pane→name source the server reads, registration or not).
        eprintln!("flywheel: mail register_agent failed for {name}: {e}");
    }
    // bridge the identity to the herdr pane (best-effort, like register).
    // The file holds the REGISTERED identity (the mapped adjective+noun) so
    // resolve_pane_identity returns a name that exists in the registry — the
    // herdr name is not registrable and would resolve to a dead end.
    if let Err(e) = backend::run_mail(mail.write_pane_identity(&key, pane_id, &mail_name)) {
        eprintln!("flywheel: mail write_pane_identity failed for {name} on {pane_id}: {e}");
    }
}

/// Deterministic herdr-agent-name → agent-mail adjective+noun mapping.
///
/// agent-mail's `register_agent` validates names against a FIXED whitelist:
/// `VALID_ADJECTIVES` (75) × `VALID_NOUNS` (132), lowercase-split + PascalCase
/// (bd-odc; verified from `mcp_agent_mail_core::models`). Any other shape is
/// rejected, INCLUDING lookalikes like `FrostDolphin` (dolphin isn't in the
/// noun list). This maps any herdr name (`flywheel-flywheel-oc`) to a stable
/// adjective+noun from the SAME whitelist via FNV-1a hash into the lists.
///
/// The mapping is deterministic — the same herdr name always maps to the
/// same agent-mail name, across runs and machines — so re-registration is
/// idempotent and the reverse lookup (herdr name in task_description) is
/// stable. Collisions (different herdr names → same pair) are possible but
/// harmless: register_agent is an upsert per name, and the herdr name is
/// preserved in task_description, so no information is lost.
///
/// Lists copied verbatim from mcp_agent_mail_rust
/// crates/mcp-agent-mail-core/src/models.rs (Apache/MIT). If the server
/// updates its lists, flywheel's copy must follow or registration breaks.
/// If `s` lowercases to a VALID_ADJECTIVES+VALID_NOUNS pair (the server's
/// whitelist shape, PascalCase after normalization), return the canonical
/// PascalCase form; else None. This is the identity gate for
/// [`agent_mail_name`]: an input already shaped as a pair passes through.
fn valid_pair(s: &str, adjectives: &[&str], nouns: &[&str]) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    for adj in adjectives {
        if let Some(rest) = lower.strip_prefix(adj) && !rest.is_empty() && nouns.contains(&rest) {
            let mut adj = (*adj).to_owned();
            adj.get_mut(0..1)?.make_ascii_uppercase();
            let mut noun = rest.to_owned();
            noun.get_mut(0..1)?.make_ascii_uppercase();
            return Some(format!("{adj}{noun}"));
        }
    }
    None
}

fn agent_mail_name(herdr_name: &str) -> String {
    // idempotency guard: an input already shaped as a whitelist pair (e.g. a
    // previously mapped name) passes through unchanged — re-mapping a mapped
    // name must be a no-op, not a second hash into a DIFFERENT pair.
    if let Some(pair) = valid_pair(herdr_name, &ADJECTIVES, &NOUNS) {
        return pair;
    }
    // Keep in lockstep with mcp_agent_mail_core::models::VALID_ADJECTIVES.
    const ADJECTIVES: [&str; 75] = [
        "red",
        "orange",
        "yellow",
        "pink",
        "black",
        "purple",
        "blue",
        "brown",
        "white",
        "green",
        "chartreuse",
        "lilac",
        "fuchsia",
        "azure",
        "amber",
        "coral",
        "crimson",
        "cyan",
        "gold",
        "golden",
        "gray",
        "indigo",
        "ivory",
        "jade",
        "lavender",
        "magenta",
        "maroon",
        "navy",
        "olive",
        "pearl",
        "rose",
        "ruby",
        "sage",
        "scarlet",
        "silver",
        "teal",
        "topaz",
        "violet",
        "cobalt",
        "copper",
        "bronze",
        "emerald",
        "sapphire",
        "turquoise",
        "beige",
        "tan",
        "cream",
        "peach",
        "plum",
        "sunny",
        "misty",
        "foggy",
        "stormy",
        "windy",
        "frosty",
        "dusty",
        "hazy",
        "cloudy",
        "rainy",
        "snowy",
        "icy",
        "mossy",
        "sandy",
        "swift",
        "quiet",
        "bold",
        "calm",
        "bright",
        "dark",
        "wild",
        "silent",
        "gentle",
        "rustic",
        "noble",
        "proud",
    ];
    // Keep in lockstep with mcp_agent_mail_core::models::VALID_NOUNS.
    const NOUNS: [&str; 132] = [
        "stone",
        "lake",
        "creek",
        "pond",
        "mountain",
        "hill",
        "snow",
        "castle",
        "river",
        "forest",
        "valley",
        "canyon",
        "meadow",
        "prairie",
        "desert",
        "island",
        "cliff",
        "cave",
        "glacier",
        "waterfall",
        "spring",
        "stream",
        "reef",
        "dune",
        "ridge",
        "peak",
        "gorge",
        "marsh",
        "brook",
        "glen",
        "grove",
        "fern",
        "hollow",
        "basin",
        "cove",
        "bay",
        "harbor",
        "coast",
        "shore",
        "bluff",
        "knoll",
        "summit",
        "plateau",
        "dog",
        "cat",
        "bear",
        "fox",
        "wolf",
        "deer",
        "elk",
        "moose",
        "otter",
        "beaver",
        "badger",
        "lynx",
        "puma",
        "squirrel",
        "rabbit",
        "hare",
        "mouse",
        "mink",
        "seal",
        "horse",
        "lion",
        "tiger",
        "panther",
        "leopard",
        "jaguar",
        "coyote",
        "bison",
        "ox",
        "hawk",
        "eagle",
        "owl",
        "falcon",
        "raven",
        "heron",
        "crane",
        "finch",
        "robin",
        "sparrow",
        "duck",
        "goose",
        "swan",
        "dove",
        "wren",
        "jay",
        "lark",
        "kite",
        "condor",
        "osprey",
        "pelican",
        "gull",
        "tern",
        "stork",
        "ibis",
        "cardinal",
        "oriole",
        "thrush",
        "trout",
        "salmon",
        "bass",
        "pike",
        "carp",
        "turtle",
        "frog",
        "pine",
        "oak",
        "maple",
        "birch",
        "cedar",
        "willow",
        "aspen",
        "elm",
        "orchid",
        "lotus",
        "ivy",
        "tower",
        "bridge",
        "forge",
        "mill",
        "barn",
        "gate",
        "anchor",
        "lantern",
        "beacon",
        "compass",
        "horizon",
        "spire",
        "chapel",
        "citadel",
        "fortress",
    ];
    let h = state::fnv1a64(herdr_name);
    let adj = ADJECTIVES[(h % ADJECTIVES.len() as u64) as usize];
    let noun = NOUNS[((h >> 32) % NOUNS.len() as u64) as usize];
    // PascalCase: adjective + noun, each capitalized (server normalizes to
    // PascalCase before storing, so any case is accepted; produce the canonical
    // form).
    let mut adj = adj.to_owned();
    let mut noun = noun.to_owned();
    if let Some(c) = adj.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    if let Some(c) = noun.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    format!("{adj}{noun}")
}

/// Resolve a mail-recipient specifier to a registered agent-mail identity
/// name. A herdr agent name (`flywheel-<base>-<kind>[N]`) maps
/// deterministically to its adjective+noun identity — registration uses the
/// same mapping (bd-odc), so the identity exists. Anything else passes
/// through unchanged: it is either an already-valid identity (the mapping is
/// idempotent for those) or a recipient the server will reject honestly.
/// Pure — no registry round trips.
fn resolve_mail_target(target: &str) -> String {
    if target.contains('-') {
        agent_mail_name(target)
    } else {
        target.to_string()
    }
}

#[cfg(test)]
mod agent_mail_name_tests {
    use super::*;

    /// 193.1: a non-UTF-8 path converts to an actionable ERROR (never a
    /// panic, never silent lossy corruption) — the herdr/mail wire protocols
    /// are UTF-8, so a non-UTF-8 path is a clear failure.
    #[test]
    fn utf8_path_rejects_non_utf8_with_error() {
        use std::ffi::OsString;
        #[cfg(unix)]
        use std::os::unix::ffi::OsStringExt;
        // byte-string path: 0x66 'f', 0xFF (invalid UTF-8), "oo"
        let bad = PathBuf::from(OsString::from_vec(vec![0x66, 0xFF, 0x6F, 0x6F]));
        #[cfg(not(unix))]
        let bad = PathBuf::from("plain-utf8"); // non-unix can't build non-UTF-8 paths
        let err = utf8_path(&bad, "test path").unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "actionable error: {err:#}");
        // a valid path round-trips exactly
        let good = PathBuf::from("/tmp/flywheel-utf8-ok");
        assert_eq!(utf8_path(&good, "test path").unwrap(), "/tmp/flywheel-utf8-ok");
    }

    /// The mapping is deterministic: same herdr name → same adjective+noun,
    /// every call (and across machines — no randomness, no time).
    #[test]
    fn mapping_is_deterministic() {
        let a = agent_mail_name("flywheel-flywheel-oc");
        let b = agent_mail_name("flywheel-flywheel-oc");
        assert_eq!(a, b);
        assert_eq!(a, agent_mail_name("flywheel-flywheel-oc"));
    }

    /// The output is a valid agent-mail name per the server contract
    /// (mcp_agent_mail_core::models::normalize_agent_name): a VALID_ADJECTIVES
    /// member + VALID_NOUNS member, total length >= 5. This is the REAL gate
    /// — bd-odc proved lookalikes like FrostDolphin are rejected.
    #[test]
    fn mapping_produces_valid_agent_mail_name() {
        for name in [
            "flywheel-flywheel-oc",
            "flywheel-hachiko-pi2",
            "flywheel-toron-gk3",
            "oc",
            "pi",
            "gk",
        ] {
            let m = agent_mail_name(name);
            let lower = m.to_ascii_lowercase();
            assert!(
                lower.len() >= 5,
                "{name} -> '{m}' too short for normalize_agent_name (>=5)"
            );
            // split at every possible adjective boundary and check membership
            // (mirrors split_valid_agent_name's lazy scan)
            let adjectives = [
                "red",
                "blue",
                "green",
                "golden",
                "sapphire",
                "turquoise",
                "emerald",
                "crimson",
                "frosty",
                "misty",
                "stormy",
                "swift",
                "quiet",
                "bold",
                "calm",
                "bright",
                "dark",
                "wild",
                "silent",
                "gentle",
                "rustic",
                "noble",
                "proud",
                "amber",
                "coral",
                "azure",
                "teal",
                "topaz",
                "violet",
                "pearl",
                "ruby",
                "jade",
                "ivory",
                "indigo",
                "lilac",
                "fuchsia",
                "maroon",
                "navy",
                "olive",
                "sage",
                "scarlet",
                "silver",
                "copper",
                "bronze",
                "beige",
                "cream",
                "peach",
                "plum",
                "sunny",
                "foggy",
                "hazy",
                "cloudy",
                "rainy",
                "snowy",
                "icy",
                "mossy",
                "sandy",
                "gray",
                "black",
                "white",
                "brown",
                "pink",
                "yellow",
                "orange",
                "chartreuse",
                "lavender",
                "magenta",
                "rose",
                "cobalt",
                "tan",
            ];
            let nouns = [
                "stone",
                "lake",
                "creek",
                "pond",
                "mountain",
                "hill",
                "snow",
                "castle",
                "river",
                "forest",
                "valley",
                "canyon",
                "meadow",
                "prairie",
                "desert",
                "island",
                "cliff",
                "cave",
                "glacier",
                "waterfall",
                "spring",
                "stream",
                "reef",
                "dune",
                "ridge",
                "peak",
                "gorge",
                "marsh",
                "brook",
                "glen",
                "grove",
                "fern",
                "hollow",
                "basin",
                "cove",
                "bay",
                "harbor",
                "coast",
                "shore",
                "bluff",
                "knoll",
                "summit",
                "plateau",
                "dog",
                "cat",
                "bear",
                "fox",
                "wolf",
                "deer",
                "elk",
                "moose",
                "otter",
                "beaver",
                "badger",
                "lynx",
                "puma",
                "squirrel",
                "rabbit",
                "hare",
                "mouse",
                "mink",
                "seal",
                "horse",
                "lion",
                "tiger",
                "panther",
                "leopard",
                "jaguar",
                "coyote",
                "bison",
                "ox",
                "hawk",
                "eagle",
                "owl",
                "falcon",
                "raven",
                "heron",
                "crane",
                "finch",
                "robin",
                "sparrow",
                "duck",
                "goose",
                "swan",
                "dove",
                "wren",
                "jay",
                "lark",
                "kite",
                "condor",
                "osprey",
                "pelican",
                "gull",
                "tern",
                "stork",
                "ibis",
                "cardinal",
                "oriole",
                "thrush",
                "trout",
                "salmon",
                "bass",
                "pike",
                "carp",
                "turtle",
                "frog",
                "pine",
                "oak",
                "maple",
                "birch",
                "cedar",
                "willow",
                "aspen",
                "elm",
                "orchid",
                "lotus",
                "ivy",
                "tower",
                "bridge",
                "forge",
                "mill",
                "barn",
                "gate",
                "anchor",
                "lantern",
                "beacon",
                "compass",
                "horizon",
                "spire",
                "chapel",
                "citadel",
                "fortress",
            ];
            let adj_ok = adjectives.iter().any(|a| {
                lower
                    .strip_prefix(a)
                    .is_some_and(|rest| nouns.contains(&rest) && !rest.is_empty())
            });
            assert!(
                adj_ok,
                "{name} -> '{m}' must be a VALID_ADJECTIVES+VALID_NOUNS pair"
            );
        }
    }

    /// The mapping spreads across the tables (not all names collapse to one
    /// adjective+noun) — collisions are harmless but rare is better.
    #[test]
    fn mapping_has_spread() {
        let names = [
            "flywheel-flywheel-oc",
            "flywheel-flywheel-oc2",
            "flywheel-hachiko-pi",
            "flywheel-hachiko-pi2",
            "flywheel-toron-gk",
            "flywheel-toron-gk2",
            "flywheel-chiebukuro-oc",
            "flywheel-chiebukuro-pi",
            "flywheel-chiebukuro-gk",
        ];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for n in names {
            seen.insert(agent_mail_name(n));
        }
        assert!(
            seen.len() >= 6,
            "expected spread across adjective+noun pairs, got {} unique: {seen:?}",
            seen.len()
        );
    }

    /// Idempotency: a name already shaped as a whitelist pair maps to ITSELF
    /// — re-mapping a mapped name must be a no-op, not a second hash into a
    /// DIFFERENT pair (the reverse-mapping gap: send paths that receive a
    /// mail identity must never re-map it).
    #[test]
    fn mapping_is_idempotent_on_valid_pairs() {
        for name in [
            "TopazOx",
            "SapphireOwl",
            "CoralFinch",
            "MistyCardinal",
            "AzurePine",
        ] {
            assert_eq!(
                agent_mail_name(name),
                name,
                "{name} must pass through unchanged"
            );
        }
    }

    /// The shared splitter returns the canonical PascalCase form and rejects
    /// anything that is not a whitelist pair (regardless of input case).
    #[test]
    fn valid_pair_splits_and_canonicalizes() {
        let adjs = ["topaz", "sapphire"];
        let nouns = ["ox", "owl"];
        assert_eq!(
            valid_pair("TopazOx", &adjs, &nouns),
            Some("TopazOx".to_string())
        );
        assert_eq!(
            valid_pair("sapphireowl", &adjs, &nouns),
            Some("SapphireOwl".to_string())
        );
        // any adjective x any noun is a valid pair (topaz + owl both listed)
        assert_eq!(
            valid_pair("topazowl", &adjs, &nouns),
            Some("TopazOwl".to_string())
        );
        assert_eq!(
            valid_pair("topaz", &adjs, &nouns),
            None,
            "adjective alone is invalid"
        );
        assert_eq!(
            valid_pair("ox", &adjs, &nouns),
            None,
            "noun alone is invalid"
        );
        assert_eq!(valid_pair("flywheel-flywheel-oc", &adjs, &nouns), None);
        assert_eq!(valid_pair("", &adjs, &nouns), None);
    }

    /// A herdr agent name maps to its identity; anything already a valid
    /// identity (or a bare short like "oc") passes through untouched.
    #[test]
    fn resolve_mail_target_maps_herdr_names_and_passes_through_others() {
        assert_eq!(
            resolve_mail_target("flywheel-flywheel-oc"),
            agent_mail_name("flywheel-flywheel-oc")
        );
        assert_eq!(resolve_mail_target("TopazOx"), "TopazOx");
        assert_eq!(resolve_mail_target("oc"), "oc");
        assert_eq!(resolve_mail_target(""), "");
    }

    /// review-fix F7: bare shorts resolve to the spawned herdr name; concrete
    /// names pass through unchanged.
    #[test]
    fn resolve_agent_default_expands_shorts_and_passes_concrete() {
        // bare shorts expand to the first-ordinal spawned name
        assert_eq!(
            resolve_agent_default("hachiko", "oc"),
            "flywheel-hachiko-oc"
        );
        assert_eq!(
            resolve_agent_default("hachiko", "pi"),
            "flywheel-hachiko-pi"
        );
        assert_eq!(
            resolve_agent_default("hachiko", "gk"),
            "flywheel-hachiko-gk"
        );
        // explicit spawned names pass through
        assert_eq!(
            resolve_agent_default("hachiko", "flywheel-hachiko-oc2"),
            "flywheel-hachiko-oc2"
        );
        // an already-mapped adjective+noun identity passes through
        let mapped = agent_mail_name("flywheel-hachiko-oc");
        assert_eq!(resolve_agent_default("hachiko", &mapped), mapped);
        // raw pane id passes through
        assert_eq!(resolve_agent_default("hachiko", "w1:p1"), "w1:p1");
        // empty stays empty (caller validates distinctness)
        assert_eq!(resolve_agent_default("hachiko", ""), "");
    }

    /// bd-bwf: full herdr names shown by `flywheel list` must NOT be
    /// double-wrapped. Bare shorts still expand.
    #[test]
    fn to_want_token_passes_through_full_names() {
        // full spawned name passes through (the fix for bd-bwf)
        assert_eq!(
            to_want_token("toron", "flywheel-toron-oc"),
            "flywheel-toron-oc"
        );
        assert_eq!(
            to_want_token("toron", "flywheel-toron-oc2"),
            "flywheel-toron-oc2"
        );
        // bare shorts expand to first ordinal
        assert_eq!(to_want_token("toron", "oc"), "flywheel-toron-oc");
        assert_eq!(to_want_token("toron", "pi"), "flywheel-toron-pi");
        // raw pane id passes through
        assert_eq!(to_want_token("toron", "w1:p1"), "w1:p1");
        // empty passes through (caller validates distinctness)
        assert_eq!(to_want_token("toron", ""), "");
    }
}

// ---- add ----

fn cmd_add(
    cfg: &Config,
    project: &str,
    counts: &AgentCounts,
    kinds: &[(String, u32)],
) -> Result<()> {
    let plan = recipe_counts(None, counts, kinds)?;
    let dir = project_dir(cfg, project);
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let existing: Vec<AgentInfo> = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    let panes = c.pane_list(Some(&ws_id))?;
    let root = panes
        .first()
        .ok_or_else(|| anyhow!("workspace {ws_id} has no panes"))?;
    // seed per-kind ordinals from existing agents so added names stay unique
    let mut ordinals: BTreeMap<String, u32> = BTreeMap::new();
    for a in &existing {
        let Some(name) = a.name.as_deref() else {
            continue;
        };
        for (short, _) in &plan {
            if let Some(o) = agent_ordinal(name, project, short) {
                ordinals
                    .entry(short.clone())
                    .and_modify(|m| *m = (*m).max(o))
                    .or_insert(o);
            }
        }
    }
    let mut prev = root.pane_id.clone();
    let mut spawned: Vec<Value> = Vec::new();
    for (short, n) in &plan {
        let kind = cfg.resolve_kind(short);
        for _ in 0..*n {
            let ordinal = ordinals.entry(short.clone()).or_insert(0);
            *ordinal += 1;
            let name = agent_name_n(project, short, *ordinal);
            let p = herdr_shim::pane_split(
                &mut c,
                &prev,
                "down",
                Some(&utf8_path(&dir, "workspace dir")?),
            )?;
            prev = p.pane_id.clone();
            let agent = start_agent_with_retry(&mut c, &name, &kind, &prev, &[])?;
            register_spawned_agent(cfg, project, &name, &kind, &agent.pane_id);
            println!("  {} {}", kind_paint(&kind), agent.pane_id);
            spawned
                .push(json!({ "name": agent.name, "kind": agent.agent, "pane_id": agent.pane_id }));
        }
    }
    log(
        project,
        EventKind::Add,
        json!({ "workspace_id": ws_id, "agents": spawned }),
    );
    Ok(())
}

// ---- list / status ----

fn cmd_list(cfg: &Config, all: bool) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let workspaces = c.workspace_list()?;
    let agents = c.agent_list()?;
    let mut rows: Vec<Value> = Vec::new();
    for ws in workspaces
        .iter()
        .filter(|w| all || w.label.starts_with("flywheel-"))
    {
        let ws_agents: Vec<Value> = agents
            .iter()
            .filter(|a| a.workspace_id == ws.workspace_id)
            .map(|a| json!({ "name": a.name, "kind": a.agent, "status": a.agent_status.as_str() }))
            .collect();
        rows.push(json!({
            "workspace_id": ws.workspace_id,
            "label": ws.label,
            "cwd": ws.cwd,
            "pane_count": ws.pane_count,
            "agents": ws_agents
        }));
    }
    if std::io::stdout().is_terminal() {
        for r in &rows {
            println!("{}", r["label"].as_str().unwrap_or("?"));
            if let Some(ags) = r["agents"].as_array() {
                for a in ags {
                    let st = a["status"].as_str().unwrap_or("unknown");
                    let st = herdr_shim::parse_agent_status(st)
                        .map(status_paint)
                        .unwrap_or_else(|_| dim(st));
                    println!(
                        "  {}  {}  {}",
                        dim(a["name"].as_str().unwrap_or("?")),
                        kind_paint(a["kind"].as_str().unwrap_or("?")),
                        st
                    );
                }
            }
            println!();
        }
        if rows.is_empty() {
            println!("no flywheel swarms (run: flywheel spawn <project> --oc=1 --pi=1 --gk=1)");
        }
    } else {
        print_json(&json!({ "workspaces": rows }));
    }
    Ok(())
}

fn cmd_status(cfg: &Config, project: Option<&str>) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let Some(project) = project else {
        return cmd_list(cfg, false);
    };
    let ws_id = find_workspace(&mut c, project)?;
    let agents = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect::<Vec<_>>();
    if agents.is_empty() {
        bail!("no agents in {project} — is the swarm spawned?");
    }
    let rows: Vec<Value> = agents
        .iter()
        .map(|a| {
            json!({
                "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                "pane_id": a.pane_id, "cwd": a.cwd, "workspace_id": ws_id
            })
        })
        .collect();
    if std::io::stdout().is_terminal() {
        for r in &rows {
            let st = r["status"].as_str().unwrap_or("unknown");
            let st = herdr_shim::parse_agent_status(st)
                .map(status_paint)
                .unwrap_or_else(|_| dim(st));
            println!(
                "{}  {}  {}  {}",
                kind_paint(r["kind"].as_str().unwrap_or("?")),
                dim(r["name"].as_str().unwrap_or("?")),
                st,
                dim(r["pane_id"].as_str().unwrap_or(""))
            );
        }
    } else {
        print_json(&json!({ "workspace_id": ws_id, "agents": rows }));
    }
    Ok(())
}

// ---- send ----

#[allow(clippy::too_many_arguments)] // clap command handler: one param per CLI flag.
fn cmd_send(
    cfg: &Config,
    project: &str,
    msg: Option<&str>,
    to: &str,
    stage: Option<&str>,
    wait: bool,
    timeout_ms: Option<u64>,
    force: bool,
) -> Result<()> {
    let text = match (stage, msg) {
        (Some(s), _) => stage_prompt(s)?,
        (None, Some(m)) => m.to_string(),
        (None, None) => bail!("provide --stage NAME or a message text"),
    };

    // safety guard: refuse destructive prompts unless --force
    if !force {
        let policy = config::Policy::load()?;
        if let Some(p) = policy.violation(&text) {
            bail!(
                "refused by policy guard (pattern: {p:?}). Pass --force to override, or edit policy: flywheel policy remove"
            );
        }
    }

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;

    let timeout = timeout_ms.or_else(|| wait.then(|| cfg.timeout_ms()));
    let mut results: Vec<Value> = Vec::new();
    for a in &targets {
        // --wait means "settle": idle, done, or blocked. An empty `until` array
        // was sent before, which herdr cannot wait on meaningfully.
        let wait_opts = wait.then(|| WaitOptions {
            timeout_ms: timeout,
            until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
        });
        let agent = c.agent_prompt(agent_target(a), &text, wait_opts)?;
        let status = agent.agent_status.as_str();
        if std::io::stdout().is_terminal() {
            println!(
                "{} {}",
                dim(agent_target(a)),
                status_paint(
                    herdr_shim::parse_agent_status(status).unwrap_or(AgentStatus::Unknown)
                )
            );
        }
        results.push(json!({
            "target": agent_target(a), "status": status, "pane_id": a.pane_id
        }));
    }

    log(
        project,
        EventKind::Send,
        json!({
            "to": to, "stage": stage, "text_len": text.len(), "wait": wait, "results": results
        }),
    );
    if !std::io::stdout().is_terminal() {
        print_json(&json!({ "results": results }));
    }
    Ok(())
}

// ---- interrupt / view / watch / zoom / attach / kill ----

fn cmd_interrupt(cfg: &Config, project: &str, to: &str, keys: &[String]) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;
    let keys: Vec<&str> = if keys.is_empty() {
        vec!["Escape"]
    } else {
        keys.iter().map(|s| s.as_str()).collect()
    };
    for a in &targets {
        c.agent_send_keys(agent_target(a), &keys)?;
        if std::io::stdout().is_terminal() {
            println!("interrupted {}", dim(agent_target(a)));
        }
    }
    log(
        project,
        EventKind::Interrupt,
        json!({ "to": to, "keys": keys }),
    );
    Ok(())
}

fn cmd_view(cfg: &Config, project: &str, to: &str, lines: Option<u32>, source: &str) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;
    for a in &targets {
        let text = c.agent_read(agent_target(a), source, lines)?;
        if std::io::stdout().is_terminal() && targets.len() > 1 {
            println!("--- {} ---", agent_target(a));
        }
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

/// Resolve the display name for a watched pane from the subscription-time
/// (pane, name) pairs. The `pane.agent_status_changed` push carries the agent
/// KIND in its `agent` field — never the instance name — so event matching
/// must go through the pane id (which IS in the payload); nameless agents
/// have their pane id as the display name already.
fn watch_name_for_pane<'a>(targets: &'a [(String, String)], pane_id: &str) -> Option<&'a str> {
    targets
        .iter()
        .find(|(p, _)| p == pane_id)
        .map(|(_, n)| n.as_str())
}

fn cmd_watch(
    cfg: &Config,
    project: &str,
    to: &str,
    until: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<()> {
    let until_status: Option<AgentStatus> =
        until.map(herdr_shim::parse_agent_status).transpose()?;
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;

    // initial state
    let mut watch_names: Vec<String> = Vec::new();
    let mut watch_targets: Vec<(String, String)> = Vec::new();
    for a in resolve_agents(&mut c, &ws_id, project, to)? {
        let name = agent_target(&a).to_string();
        watch_names.push(name.clone());
        watch_targets.push((a.pane_id.clone(), name.clone()));
        let st = a.agent_status;
        if std::io::stdout().is_terminal() {
            println!("{} {}", dim(&name), status_paint(st));
        }
        if let Some(u) = until_status && st == u && all_matching(&mut c, &watch_names, u)? {
            return Ok(());
        }
    }

    // subscribe to status changes in this workspace (long-lived connection).
    // The live herdr contract requires one subscription per pane
    // (`pane.agent_status_changed` needs a concrete pane_id — a workspace
    // scope alone is rejected), so build one subscription per pane in the
    // workspace. Panes spawned after this point aren't subscribed; the poll
    // fallback below covers them on any read error.
    let panes = c.pane_list(Some(&ws_id))?;
    let subs: Vec<Subscription> = panes
        .iter()
        .map(|p| Subscription::agent_status_changed(p.pane_id.clone()))
        .collect();
    let mut stream = match herdr_shim::subscribe(&mut c, &subs) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("flywheel: WARNING subscription failed ({e:#}); polling fallback");
            None
        }
    };

    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));
    loop {
        if let Some(d) = deadline {
            if std::time::Instant::now() >= d {
                bail!("watch timed out waiting for {:?}", until_status);
            }
            // without a read timeout, a silent stream blocks forever and the
            // deadline above is never reached — bound each blocking read
            if let Some(s) = stream.as_mut() {
                s.set_read_timeout(Some(d.saturating_duration_since(std::time::Instant::now())))?;
            }
        } else if let Some(s) = stream.as_mut() {
            s.set_read_timeout(None)?;
        }
        match stream.as_mut() {
            Some(s) => match s.read_event() {
                Ok(Some(ev)) => {
                    // typed: the subscription push (and the equivalent server
                    // event) both land here with a fully parsed payload.
                    // The payload's `agent` field is the agent KIND, not the
                    // name — match via pane id (watch_targets).
                    match ev.data {
                        herdr_shim::HerdrEventData::PaneAgentStatusChanged(change)
                        | herdr_shim::HerdrEventData::PaneAgentStatusChangedSub(change) => {
                            let Some(name) = watch_name_for_pane(&watch_targets, &change.pane_id)
                            else {
                                continue;
                            };
                            if std::io::stdout().is_terminal() {
                                println!("{} {}", dim(name), status_paint(change.agent_status));
                            }
                            if let Some(u) = until_status
                                && change.agent_status == u
                                && all_matching(&mut c, &watch_names, u)?
                            {
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                }
                Ok(None) => bail!("herdr socket closed"),
                Err(_) => {
                    // transient read error: poll state as fallback
                    std::thread::sleep(Duration::from_millis(1000));
                    if watch_poll(&mut c, &watch_names, until_status)? {
                        return Ok(());
                    }
                }
            },
            None => {
                // no subscription (empty workspace or rejected request):
                // poll state until the deadline
                std::thread::sleep(Duration::from_millis(1000));
                if watch_poll(&mut c, &watch_names, until_status)? {
                    return Ok(());
                }
            }
        }
    }
}

/// Poll agent list as a subscription fallback: returns `true` when
/// `until_status` is reached for all watched names (or `false` while
/// polling — the caller owns the deadline).
fn watch_poll(
    c: &mut Client,
    watch_names: &[String],
    until_status: Option<AgentStatus>,
) -> Result<bool> {
    for a in c.agent_list()? {
        let name = a.name.unwrap_or(a.pane_id.clone());
        if watch_names.contains(&name)
            && let Some(u) = until_status
            && a.agent_status == u
            && all_matching(c, watch_names, u)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn all_matching(c: &mut Client, names: &[String], status: AgentStatus) -> Result<bool> {
    let agents = c.agent_list()?;
    for name in names {
        let a = agents
            .iter()
            .find(|a| a.name.as_deref() == Some(name) || a.pane_id == *name);
        match a {
            Some(a) if a.agent_status == status => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn cmd_zoom(cfg: &Config, project: &str, to: &str, off: bool) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;
    let mode = if off { "off" } else { "on" };
    for a in &targets {
        herdr_shim::pane_zoom(&mut c, &a.pane_id, mode)?;
    }
    println!("zoom {mode} for {to}");
    Ok(())
}

fn cmd_attach(cfg: &Config, project: &str, to: &str) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agents = resolve_agents(&mut c, &ws_id, project, to)?;
    let target = agent_target(&agents[0]);
    println!("{}", dim("attaching (detach with ctrl+b q)…"));
    let status = Command::new("herdr")
        .args(["attach", target])
        .status()
        .map_err(|e| anyhow!("herdr attach failed: {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

fn cmd_kill(cfg: &Config, project: &str, force: bool) -> Result<()> {
    if !force {
        if std::io::stdin().is_terminal() {
            eprintln!("close swarm for {project}? [y/N] ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("aborted");
                return Ok(());
            }
        } else {
            // non-interactive stdin used to silently skip the confirmation —
            // automation must opt in explicitly
            bail!("refusing to close {project} without confirmation — pass --force");
        }
    }
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    // the panes die with the workspace — capture their ids first so the
    // pane-identity bridge files can be cleared (no stale lookups after
    // pane_exited, bd-mail-pane-identity-bridge)
    let agents: Vec<AgentInfo> = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    c.workspace_close(&ws_id)?;
    // tidy-up: drop the project's agent state files (stale context signals
    // must not survive into the next spawn — freshness only protects so much)
    let _ = std::fs::remove_dir_all(agentstate::project_state_dir(&project_dir(cfg, project)));
    // clear the identity files for the dead panes (best-effort — a missing
    // file is a no-op, so double-exit is harmless)
    if let Ok(mail) = backend::mail_backend(cfg) && let Some(key) = project_dir(cfg, project).to_str() {
        for a in &agents {
            if let Err(e) = backend::run_mail(mail.clear_pane_identity(key, &a.pane_id)) {
                eprintln!(
                    "flywheel: mail clear_pane_identity failed for {} on {}: {e}",
                    a.name.as_deref().unwrap_or("<unnamed>"),
                    a.pane_id
                );
            }
        }
    }
    log(project, EventKind::Close, json!({ "workspace_id": ws_id }));
    println!("closed {project}");
    Ok(())
}

// ---- work / assign ----

fn cmd_work(cfg: &Config, cmd: &WorkCmd) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let bv = work::Bv::new(cwd.to_str().unwrap_or("."));
    match cmd {
        WorkCmd::Triage => {
            let v = bv.robot("triage")?;
            print_json(&v);
        }
        WorkCmd::Next => {
            let v = bv.robot("next")?;
            print_json(&v);
        }
        WorkCmd::Insights => {
            let v = bv.robot("insights")?;
            print_json(&v);
        }
        WorkCmd::Plan => {
            let v = bv.robot("plan")?;
            print_json(&v);
        }
        WorkCmd::Ready => {
            let v = bv.br_json(&["ready"])?;
            print_json(&v);
        }
    }
    let _ = cfg;
    Ok(())
}

fn cmd_assign(cfg: &Config, project: &str, to: &str, beads: &str, msg: Option<&str>) -> Result<()> {
    let mail = mail_shim::Mail::from_config(Some(&cfg.toron_mail_config()))?.ok_or_else(|| {
        anyhow!(
            "agent-mail (toron) not configured — set [mail.toron] url in {}",
            config::config_file().display()
        )
    })?;
    let mail = mail;
    let dir = project_dir(cfg, project);
    let key = utf8_path(&dir, "project dir")?;
    mail.ensure_project(&key)?;
    let bead_ids: Vec<String> = beads
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if bead_ids.is_empty() {
        bail!("no beads given (--beads bd-1,bd-2)");
    }
    let body = format!(
        "Assigned beads: {}\n\n{}",
        bead_ids.join(", "),
        msg.unwrap_or("Execute these beads per the standard lifecycle: claim -> reserve -> execute -> verify -> close -> sync.")
    );
    let targets: Vec<String> = if to == "all" {
        let agents = mail.list_agents(&key, None)?;
        agents
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| a.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        // explicit targets: herdr agent names map to their registered
        // agent-mail identities (bd-odc); already-valid identities pass
        // through unchanged
        to.split(',')
            .map(|s| resolve_mail_target(s.trim()))
            .collect()
    };
    if targets.is_empty() {
        bail!(
            "no agent-mail identities found for {project} (register first: flywheel mail agents)"
        );
    }
    let subject = format!(
        "[assign] {} → {}",
        project_base(project),
        bead_ids.join(", ")
    );
    mail.send(&key, &targets, &subject, &body, "high", true, None)?;
    println!("assigned {} to {}", bead_ids.join(", "), targets.join(", "));
    Ok(())
}

// ---- mail ----

fn mail_or_die(cfg: &Config) -> Result<mail_shim::Mail> {
    mail_shim::Mail::from_config(Some(&cfg.toron_mail_config()))?.ok_or_else(|| {
        anyhow!(
            "toron mail not configured — set [mail.toron] url in {} (AM fallback: mail=agentmail)",
            config::config_file().display()
        )
    })
}

/// Who this CLI mail invocation acts as (bd-mail-actor-identity-nze).
///
/// Priority: explicit `--as` → `FLYWHEEL_MAIL_AS` → `AGENT_NAME` → config
/// `[agent_mail].name` (orchestrator CoralFinch). Without this, every crew
/// `flywheel mail send` collapsed to CoralFinch and peer collab was unreadable.
fn resolve_mail_actor(explicit: Option<&str>) -> Option<String> {
    if let Some(a) = explicit {
        let t = a.trim();
        if !t.is_empty() {
            return Some(resolve_mail_target(t));
        }
    }
    for key in ["FLYWHEEL_MAIL_AS", "AGENT_NAME"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(resolve_mail_target(t));
            }
        }
    }
    None
}

/// Mail facade acting as resolved actor (registers identity best-effort).
fn mail_as(cfg: &Config, project_key: &str, as_agent: Option<&str>) -> Result<mail_shim::Mail> {
    let mail = mail_or_die(cfg)?;
    let mail = match resolve_mail_actor(as_agent) {
        Some(name) => mail.with_name(name),
        None => mail,
    };
    // Ensure the actor exists so send/reserve/inbox don't 404 "Agent not found".
    let name = mail.name.clone();
    if let Err(e) = mail.register_agent(
        project_key,
        &name,
        "flywheel",
        "flywheel",
        "flywheel mail CLI actor",
    ) {
        eprintln!("flywheel: mail self-register as {name} failed (non-fatal): {e:#}");
    }
    Ok(mail)
}

/// Parsed cross-project swarm request (subject `[xproj] …` + body fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XprojRequest {
    pub recipe: String,
    pub workers: u32,
    pub params: String,
    pub product: String,
}

/// One `[xproj]` inbox hit for the durable loop (bd-pattern-cross-project-swarm).
///
/// `id` is a String because the mail backend's id type varies: agent-mail
/// emits u64 sequence numbers, toron emits hex event ids. Accept both on
/// deserialize so the xproj fan-out works against either backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XprojInboxItem {
    #[serde(deserialize_with = "de_id_string")]
    pub id: String,
    pub subject: String,
    pub body: String,
    pub thread_id: String,
    pub from: String,
}

/// Deserialize a message id that may arrive as a JSON number (agent-mail) or
/// a string (toron hex event id). Both normalize to a string.
pub fn de_id_string<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(D::Error::custom("message id must be a string or number")),
    }
}

/// Build the worker queue prompt for a cross-project request.
pub fn xproj_queue_prompt(item: &XprojInboxItem, req: &XprojRequest) -> String {
    format!(
        "CROSS-PROJECT SWARM REQUEST (message_id={}, thread={}, from={}).\n\
         recipe={} workers={} product={}.\n\
         Params:\n{}\n\n\
         Run this recipe/workflow in THIS project (spawn workers if needed). \
         When finished, reply a concise summary in agent-mail thread `{}` \
         (reply_message / send with same thread_id) so the requester can read it. \
         Acknowledge the request message if ack_required.",
        item.id,
        item.thread_id,
        item.from,
        req.recipe,
        req.workers,
        req.product,
        req.params,
        item.thread_id
    )
}

/// Extract `[xproj]` items from a `fetch_inbox` JSON value (pure, unit-tested).
/// Accepts either a bare array or `{messages:[...]}` / `{result:[...]}`.
pub fn extract_xproj_from_inbox(v: &serde_json::Value) -> Vec<XprojInboxItem> {
    let arr = v
        .as_array()
        .or_else(|| v.get("messages").and_then(|m| m.as_array()))
        .or_else(|| v.get("result").and_then(|m| m.as_array()))
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for m in arr {
        let subject = m
            .get("subject")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let body = m
            .get("body_md")
            .or_else(|| m.get("body"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let combined = format!("{subject}\n{body}");
        if !combined.to_ascii_lowercase().contains("[xproj]") {
            continue;
        }
        // id: agent-mail emits numbers, toron emits hex strings — accept both.
        let id = match m.get("id") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let thread_id = m
            .get("thread_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let from = m
            .get("from")
            .or_else(|| m.get("sender"))
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        out.push(XprojInboxItem {
            id,
            subject,
            body,
            thread_id,
            from,
        });
    }
    out
}

/// Filter unseen xproj items and produce (message_id, queue_prompt) pairs.
pub fn xproj_prompts_for_unseen(
    items: &[XprojInboxItem],
    seen: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in items {
        if seen.contains(&item.id) {
            continue;
        }
        let combined = format!("{}\n{}", item.subject, item.body);
        if let Some(req) = parse_xproj_request(&combined) {
            out.push((item.id.clone(), xproj_queue_prompt(item, &req)));
        }
    }
    out
}

/// Parse an `[xproj]` swarm request from subject and/or body.
///
/// Accepts lines like `recipe=backtest-5m`, `workers=4`, `product=trading-research`,
/// free-form remainder as params. Subject may be `[xproj] run … recipe=foo`.
pub fn parse_xproj_request(text: &str) -> Option<XprojRequest> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // require [xproj] marker somewhere (case-insensitive)
    if !t.to_ascii_lowercase().contains("[xproj]") {
        return None;
    }
    let mut recipe = String::new();
    let mut workers = 2u32;
    let mut product = "trading-research".to_string();
    let mut params_parts: Vec<String> = Vec::new();
    for raw in t.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("recipe=") {
            recipe = line[line.len() - rest.len()..].to_string();
            continue;
        }
        if let Some(rest) = lower.strip_prefix("workers=") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                workers = n.max(1);
            }
            continue;
        }
        if let Some(rest) = lower.strip_prefix("product=") {
            product = line[line.len() - rest.len()..].to_string();
            continue;
        }
        // skip pure [xproj] subject lines without params
        if lower.starts_with("[xproj]") && !line.contains('=') {
            // maybe " [xproj] run 4-worker … recipe=foo" on one line
            for tok in line.split_whitespace() {
                let tl = tok.to_ascii_lowercase();
                if let Some(r) = tl.strip_prefix("recipe=") {
                    recipe = tok[tok.len() - r.len()..].to_string();
                }
            }
            continue;
        }
        params_parts.push(line.to_string());
    }
    if recipe.is_empty() {
        // last chance: recipe= anywhere in full text
        for tok in t.split_whitespace() {
            let tl = tok.to_ascii_lowercase();
            if let Some(r) = tl.strip_prefix("recipe=") {
                recipe = tok[tok.len() - r.len()..].to_string();
                break;
            }
        }
    }
    if recipe.is_empty() {
        return None;
    }
    Some(XprojRequest {
        recipe,
        workers,
        params: params_parts.join("\n"),
        product,
    })
}

fn cmd_xproj(cfg: &Config, cmd: &XprojCmd) -> Result<()> {
    match cmd {
        XprojCmd::Request {
            project,
            to,
            to_project,
            reason,
        } => {
            let mail = mail_or_die(cfg)?;
            let key = project_dir_str(cfg, project)?;
            let to_key = project_dir_str(cfg, to_project)?;
            mail.ensure_project(&key)?;
            let from = mail.name.clone();
            let to_mail = resolve_mail_target(to);
            let v = mail.request_contact(&key, &from, &to_mail, Some(&to_key), reason, None)?;
            print_json(&v);
        }
        XprojCmd::Send {
            project,
            to,
            to_project,
            recipe,
            body,
            product,
            workers,
        } => {
            let mail = mail_or_die(cfg)?;
            let key = project_dir_str(cfg, project)?;
            mail.ensure_project(&key)?;
            let to_mail = resolve_mail_target(to);
            let ts = crate::state::now_iso().replace(['-', ':'], "");
            let thread = format!("xproj-{recipe}-{ts}");
            let subject = format!("[xproj] run {workers}-worker swarm recipe={recipe}");
            // review-fix F3: `send_message` has NO to_project param — agent-mail
            // delivers by recipient NAME across projects once the contact link is
            // approved (see `xproj request --to-project` for the handshake; the
            // receiver's loop polls ITS OWN project inbox via __xproj-inbox).
            // `to_project` is therefore routing CONTEXT, not a delivery target:
            // validate the path, carry it in the body so the receiver + logs have
            // full context, and surface it in the JSON. Never silently drop it.
            let to_project_validated: Option<String> = if to_project.is_empty() {
                None
            } else {
                let p = project_dir(cfg, to_project);
                if !p.is_dir() {
                    eprintln!(
                        "flywheel: warning: --to-project {} is not a directory — delivery is name-based; the contact handshake (xproj request --to-project) is what authorizes cross-project routing",
                        p.display()
                    );
                }
                Some(to_project.to_string())
            };
            let body_md = format!(
                "recipe={recipe}\nworkers={workers}\nproduct={product}\n{}\n\n{body}\n\n\
                 Reply results in this thread. Receiver: spawn workers in your project, \
                 run the recipe/workflow, reply a summary here.",
                to_project_validated
                    .as_ref()
                    .map(|tp| format!("to_project={tp}"))
                    .unwrap_or_default()
            );
            let v = mail.send(
                &key,
                &[to_mail],
                &subject,
                &body_md,
                "high",
                true,
                Some(&thread),
            )?;
            print_json(&json!({
                "ok": true,
                "thread_id": thread,
                "subject": subject,
                "product": product,
                "recipe": recipe,
                "workers": workers,
                "to_project": to_project_validated,
                "delivery": v,
            }));
        }
        XprojCmd::Parse { text } => match parse_xproj_request(text) {
            Some(r) => print_json(&json!({
                "ok": true,
                "recipe": r.recipe,
                "workers": r.workers,
                "product": r.product,
                "params": r.params,
            })),
            None => {
                print_json(&json!({
                    "ok": false,
                    "error": "not an [xproj] request or missing recipe=",
                }));
            }
        },
    }
    Ok(())
}

fn cmd_mail(cfg: &Config, cmd: &MailCmd) -> Result<()> {
    match cmd {
        MailCmd::Inbox {
            project,
            urgent,
            limit,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.inbox(&key, &name, *urgent, *limit)?;
            print_json(&v);
        }
        MailCmd::Send {
            project,
            to,
            subject,
            body,
            importance,
            ack,
            as_agent,
            thread_id,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            // herdr names resolve to their registered identities (bd-odc)
            let targets: Vec<String> = to
                .split(',')
                .map(|s| resolve_mail_target(s.trim()))
                .collect();
            let v = mail.send(
                &key,
                &targets,
                subject,
                body,
                importance,
                *ack,
                thread_id.as_deref(),
            )?;
            print_json(&v);
        }
        MailCmd::Locks {
            project,
            paths,
            ttl_seconds,
            exclusive,
            reason,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let agent = as_agent.clone().unwrap_or_else(|| mail.name.clone());
            let v = mail.reserve(&key, &agent, paths, *ttl_seconds, *exclusive, reason)?;
            print_json(&v);
        }
        MailCmd::Unlock { project, as_agent } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            let agent = as_agent.clone().unwrap_or_else(|| mail.name.clone());
            let v = mail.release_paths(&key, &agent, &[])?;
            print_json(&v);
        }
        MailCmd::Conflicts {
            project,
            paths,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let agent = as_agent.clone().unwrap_or_else(|| mail.name.clone());
            let v = mail.conflicts_for(&key, &agent, paths)?;
            print_json(&v);
        }
        MailCmd::Agents { project } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.list_agents(&key, None)?;
            print_json(&v);
        }
        MailCmd::Contact { cmd } => match cmd {
            MailContactCmd::Request {
                project,
                to,
                to_project,
                reason,
            } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                mail.ensure_project(&key)?;
                let from = mail.name.clone();
                let to_mail = resolve_mail_target(to);
                let to_proj = to_project
                    .as_ref()
                    .map(|p| project_dir_str(cfg, p))
                    .transpose()?;
                let v =
                    mail.request_contact(&key, &from, &to_mail, to_proj.as_deref(), reason, None)?;
                print_json(&v);
            }
            MailContactCmd::Approve { project, from } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let to = mail.name.clone();
                let from_mail = resolve_mail_target(from);
                let v = mail.respond_contact(&key, &to, &from_mail, true)?;
                print_json(&v);
            }
            MailContactCmd::Deny { project, from } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let to = mail.name.clone();
                let from_mail = resolve_mail_target(from);
                let v = mail.respond_contact(&key, &to, &from_mail, false)?;
                print_json(&v);
            }
            MailContactCmd::List { project } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let name = mail.name.clone();
                let v = mail.list_contacts(&key, &name)?;
                print_json(&v);
            }
            MailContactCmd::Policy { project, policy } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, None)?;
                let name = mail.name.clone();
                let v = mail.set_contact_policy(&key, &name, policy)?;
                print_json(&v);
            }
        },
        MailCmd::Product { cmd } => match cmd {
            MailProductCmd::Ensure { name } => {
                let mail = mail_or_die(cfg)?;
                let v = mail.ensure_product(name, Some(name))?;
                print_json(&v);
            }
            MailProductCmd::Link { product, project } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_or_die(cfg)?;
                mail.ensure_project(&key)?;
                let v = mail.products_link(product, &key)?;
                print_json(&v);
            }
            MailProductCmd::Search {
                product,
                query,
                limit,
            } => {
                let mail = mail_or_die(cfg)?;
                let v = mail.search_product(product, query, *limit)?;
                print_json(&v);
            }
            MailProductCmd::Inbox { product, limit } => {
                let mail = mail_or_die(cfg)?;
                let name = mail.name.clone();
                let v = mail.inbox_product(product, &name, *limit)?;
                print_json(&v);
            }
        },
    }
    Ok(())
}

// ---- toron (in-process embed) ----

/// `flywheel toron <subcommand>`: drive the in-process toron mailbox / MCP
/// server (ADR-0003 Decision 3A, bd-o8j.1). The embed is async (toron-mcp's
/// mailbox is built over tokio), so build a fresh tokio runtime and block_on
/// the dispatch — mirroring how cmd_loop drives the durable workflow. `cfg` is
/// forwarded but unused by the embed: the demo mailbox is fully in-process.
fn cmd_toron(cfg: &Config, cmd: &ToronCmd) -> Result<()> {
    // The MCP-config wiring is pure file I/O — it runs synchronously and does
    // not need the tokio embed runtime. Handle it before delegating to the
    // in-process mailbox dispatch.
    if let ToronCmd::SetupMcp {
        url,
        agents,
        boot_port,
        dry_run,
        flywheel,
    } = cmd
    {
        return toron_setup_mcp(url.as_deref(), agents, *boot_port, *dry_run, *flywheel);
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(toron_embed::dispatch(cfg, cmd))
}

// ---- worktree ----

fn cmd_worktree(cfg: &Config, cmd: &WorktreeCmd) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    match cmd {
        WorktreeCmd::List { project } => {
            let ws_id = find_workspace(&mut c, project)?;
            let v = herdr_shim::worktree_list(&mut c, Some(&ws_id))?;
            for w in &v {
                println!(
                    "{}  {}  {}",
                    kind_paint(&w.branch.clone().unwrap_or_default()),
                    w.open_workspace_id.clone().unwrap_or_default(),
                    dim(&w.path)
                );
            }
            if v.is_empty() {
                println!("no worktrees for {project}");
            }
        }
        WorktreeCmd::Create {
            project,
            branch,
            base,
            label,
        } => {
            let ws_id = find_workspace(&mut c, project)?;
            let v = c.worktree_create(&ws_id, branch, base.as_deref(), label.as_deref(), false)?;
            println!(
                "worktree {} created: {}",
                branch,
                v.get("workspace")
                    .and_then(|w| w.get("workspace_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            );
        }
        WorktreeCmd::Open { project, branch } => {
            let ws_id = find_workspace(&mut c, project)?;
            let v = c.worktree_create(&ws_id, branch, None, None, true)?;
            println!(
                "worktree {} opened: {}",
                branch,
                v.get("workspace")
                    .and_then(|w| w.get("workspace_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            );
        }
        WorktreeCmd::Remove { project, force } => {
            let ws_id = find_workspace(&mut c, project)?;
            c.worktree_remove(&ws_id, *force)?;
            println!("worktree removed for {project}");
        }
    }
    Ok(())
}

// ---- checkpoint / timeline / audit / history / changes / resume / analytics ----

fn cmd_checkpoint(cfg: &Config, cmd: &CheckpointCmd) -> Result<()> {
    match cmd {
        CheckpointCmd::Save { project, msg } => {
            let mut c = Client::connect(Some(&cfg.socket_path()))?;
            let ws_id = find_workspace(&mut c, project)?;
            let agents = c
                .agent_list()?
                .into_iter()
                .filter(|a| a.workspace_id == ws_id)
                .collect::<Vec<_>>();
            let cp_agents: Vec<state::CheckpointAgent> = agents
                .iter()
                .map(|a| state::CheckpointAgent {
                    name: a.name.clone().unwrap_or_default(),
                    kind: a.agent.clone().unwrap_or_default(),
                    status: a.agent_status,
                    pane_id: a.pane_id.clone(),
                })
                .collect();
            let path = state::save_checkpoint(
                project,
                msg,
                Some(&ws_id),
                cp_agents,
                Some(&project_dir_str(cfg, project)?),
            )?;
            log(
                project,
                EventKind::Checkpoint,
                json!({ "message": msg, "path": path.display().to_string() }),
            );
            println!("checkpoint saved: {}", dim(&path.display().to_string()));
        }
        CheckpointCmd::List { project } => {
            cmd_history(project)?;
        }
        CheckpointCmd::Show { project } => {
            if let Some(cp) = state::latest_checkpoint(project)? {
                print_json(&serde_json::to_value(&cp)?);
            } else {
                bail!("no checkpoints for {project}");
            }
        }
        CheckpointCmd::Latest { project } => {
            if let Some(cp) = state::latest_checkpoint(project)? {
                println!("{} {}  {}", dim(&cp.ts), dim(&cp.project), cp.message);
                for a in &cp.agents {
                    println!("  {}  {}", kind_paint(&a.kind), status_paint(a.status));
                }
            } else {
                bail!("no checkpoints for {project}");
            }
        }
    }
    Ok(())
}

fn cmd_timeline(project: &str) -> Result<()> {
    let entries = state::read_log(project)?;
    if entries.is_empty() {
        println!("no activity logged for {project}");
        return Ok(());
    }
    for e in entries {
        let kind = format!("{:?}", e.kind).to_lowercase();
        let detail = match e.kind {
            EventKind::Send => format!(
                "to {} stage={}",
                e.data.get("to").and_then(Value::as_str).unwrap_or("?"),
                e.data.get("stage").and_then(Value::as_str).unwrap_or("-")
            ),
            EventKind::Spawn => format!(
                "workspace={}",
                e.data
                    .get("workspace_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ),
            EventKind::Checkpoint => e
                .data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            EventKind::State => format!(
                "state={}",
                e.data.get("state").and_then(Value::as_str).unwrap_or("?")
            ),
            _ => String::new(),
        };
        println!("{} {:>12}  {}", dim(&e.ts), dim(&kind), detail);
    }
    Ok(())
}

fn cmd_audit(project: &str) -> Result<()> {
    let entries = state::read_log(project)?;
    let v: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(json!({})))
        .collect();
    print_json(&json!({ "project": project, "entries": v }));
    Ok(())
}

fn cmd_history(project: &str) -> Result<()> {
    let cps = state::list_checkpoints(project)?;
    if cps.is_empty() {
        println!("no checkpoints for {project}");
        return Ok(());
    }
    // newest first, so the displayed index matches `changes --since N`
    // (0 = latest) — history's [i] and changes' --since agree
    for (i, cp) in cps.iter().rev().enumerate() {
        println!(
            "[{}] {}  {}  {}",
            i,
            dim(&cp.ts),
            dim(&cp.project),
            cp.message
        );
    }
    Ok(())
}

fn cmd_changes(cfg: &Config, project: &str, since: usize) -> Result<()> {
    let cps = state::list_checkpoints(project)?;
    let Some(base) = cps.iter().rev().nth(since) else {
        bail!("no checkpoint at index {since} (have {})", cps.len());
    };
    println!("baseline: {} — {}", dim(&base.ts), base.message);
    // current state
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agents = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect::<Vec<_>>();
    for a in &agents {
        let prev = base
            .agents
            .iter()
            .find(|p| p.name == a.name.as_deref().unwrap_or(""));
        let prev_st = prev.map(|p| p.status.as_str()).unwrap_or("new");
        let now_st = a.agent_status.as_str();
        if prev_st != now_st {
            println!(
                "{}  {} → {}",
                dim(a.name.as_deref().unwrap_or("?")),
                dim(prev_st),
                status_paint(
                    herdr_shim::parse_agent_status(now_st).unwrap_or(AgentStatus::Unknown)
                )
            );
        }
    }
    let head = state::git_head(&project_dir_str(cfg, project)?);
    if let Ok(h) = head {
        println!("git head: {}", dim(&h));
    }
    Ok(())
}

fn cmd_resume(cfg: &Config, project: &str, recipe: Option<&str>) -> Result<()> {
    let latest = state::latest_checkpoint(project)?;
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws = match find_workspace(&mut c, project) {
        Ok(ws_id) => {
            println!("swarm alive: workspace {}", dim(&ws_id));
            Some(ws_id)
        }
        Err(_) => None,
    };
    if ws.is_none() {
        if let Some(r) = recipe {
            println!("respawn with recipe {r}…");
            cmd_spawn(
                cfg,
                project,
                &AgentCounts::default(),
                Some(r),
                &[],
                None,
                false,
                false,
            )?;
        } else {
            bail!("workspace gone (run: flywheel spawn {project} --recipe <name> or flywheel resume {project} --recipe daily)");
        }
    }
    if let Some(cp) = latest {
        println!("last checkpoint: {} — {}", dim(&cp.ts), cp.message);
        for a in &cp.agents {
            println!("  {}  {}", kind_paint(&a.kind), status_paint(a.status));
        }
    }
    log(project, EventKind::Resume, json!({}));
    Ok(())
}

fn cmd_analytics(project: &str) -> Result<()> {
    let a = state::analytics(project)?;
    print_json(&serde_json::to_value(&a)?);
    Ok(())
}

// ---- recipes / workflows / pipeline / template / ensemble ----

fn cmd_asset(cfg: &Config, kind: &str, cmd: &AssetCmd) -> Result<()> {
    match cmd {
        AssetCmd::List => {
            let names: Vec<String> = match kind {
                "recipes" | "template" => {
                    config::Recipes::load()?.recipes.keys().cloned().collect()
                }
                "workflows" => config::Workflows::load()?
                    .workflows
                    .keys()
                    .cloned()
                    .collect(),
                "pipeline" => config::Pipelines::load()?
                    .pipelines
                    .keys()
                    .cloned()
                    .collect(),
                _ => vec![],
            };
            if names.is_empty() {
                println!(
                    "no {kind} defined (edit {})",
                    config::config_dir().join(format!("{kind}.toml")).display()
                );
            } else {
                for n in names {
                    println!("{n}");
                }
            }
            Ok(())
        }
        AssetCmd::Run { name, project } => match kind {
            "recipes" | "template" => cmd_spawn(
                cfg,
                project,
                &AgentCounts::default(),
                Some(name),
                &[],
                None,
                false,
                false,
            ),
            "workflows" => run_workflow(cfg, project, name),
            "pipeline" => run_pipeline(cfg, project, name),
            _ => bail!("unknown asset kind {kind}"),
        },
    }
}

/// `flywheel stage list`: the stage library surface (ADR-0005 P3.3).
fn cmd_stage(cmd: &StageCmd) -> Result<()> {
    match cmd {
        StageCmd::List => {
            let lib = stages::StageLibrary::load()?;
            println!("stages (semantic name <-> p-code alias):");
            for (name, alias) in lib.list() {
                println!("  {name} <-> {alias}");
            }
            println!("semantic alias table (Spec 2a):");
            for (semantic, code) in lib.semantic_aliases() {
                println!("  {semantic} = {code}");
            }
            println!("override: {}", override_display());
            Ok(())
        }
    }
}

fn override_display() -> String {
    stages::override_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<none>".to_string())
}

fn run_workflow(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let workflows = config::Workflows::load()?;
    let steps = workflows
        .workflows
        .get(name)
        .ok_or_else(|| anyhow!("workflow not found: {name} (flywheel workflows list)"))?;
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    for (i, step) in steps.iter().enumerate() {
        println!("[{}/{}] {}", i + 1, steps.len(), dim(&step_label(step)));
        match step {
            WorkflowStep::Send { to, stage, msg } => {
                let text = match (stage.as_deref(), msg.as_deref()) {
                    (Some(s), _) => stage_prompt(s)?,
                    (None, Some(m)) => m.to_string(),
                    (None, None) => bail!("workflow step {i}: send needs stage or msg"),
                };
                let targets = resolve_agents(&mut c, &ws_id, project, to)?;
                for a in &targets {
                    c.agent_prompt(agent_target(a), &text, None)?;
                    println!("  {} sent", dim(agent_target(a)));
                }
            }
            WorkflowStep::Wait { until, timeout_ms } => {
                let st = herdr_shim::parse_agent_status(until)?;
                let targets = resolve_agents(&mut c, &ws_id, project, "all")?;
                for a in &targets {
                    c.agent_wait(
                        agent_target(a),
                        &[st],
                        timeout_ms.or(Some(cfg.timeout_ms())),
                    )?;
                    println!("  {} {}", dim(agent_target(a)), status_paint(st));
                }
            }
        }
    }
    log(project, EventKind::Workflow, json!({ "workflow": name }));
    println!("workflow {name} complete");
    Ok(())
}

fn step_label(step: &WorkflowStep) -> String {
    match step {
        WorkflowStep::Send { to, stage, msg } => format!(
            "send → {to} {}",
            stage
                .as_deref()
                .map(|s| format!("[{s}]"))
                .unwrap_or_else(|| msg.as_deref().unwrap_or("").chars().take(40).collect())
        ),
        WorkflowStep::Wait { until, .. } => format!("wait {until}"),
    }
}

fn run_pipeline(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let pipelines = config::Pipelines::load()?;
    let steps = pipelines
        .pipelines
        .get(name)
        .ok_or_else(|| anyhow!("pipeline not found: {name} (flywheel pipeline list)"))?;
    let workflows = config::Workflows::load()?;
    for (i, wf) in steps.iter().enumerate() {
        if !workflows.workflows.contains_key(wf) {
            bail!("pipeline {name} step {}: workflow {wf:?} not found", i + 1);
        }
        println!("[{}/{}] workflow {}", i + 1, steps.len(), dim(wf));
        run_workflow(cfg, project, wf)?;
    }
    log(project, EventKind::Pipeline, json!({ "pipeline": name }));
    println!("pipeline {name} complete");
    Ok(())
}

fn cmd_ensemble(cfg: &Config, project: &str, recipe: &str, workflow: &str) -> Result<()> {
    cmd_spawn(
        cfg,
        project,
        &AgentCounts::default(),
        Some(recipe),
        &[],
        None,
        false,
        false,
    )?;
    run_workflow(cfg, project, workflow)
}

// ---- loop (monitoring orchestrator) ----

/// What the loop should do for one agent on one poll.
#[derive(Debug, Clone, PartialEq)]
enum LoopAction {
    /// send queued instruction (popped from the queue)
    Send { name: String, text: String },
    /// send the nudge text (queue empty)
    Nudge { name: String },
    /// agent is blocked
    Blocked { name: String },
    /// nothing to do
    Wait,
}

/// Pure planning: given current agent states, decide actions.
/// Queue is drained in order across idle/done agents.
fn plan_actions(
    states: &[(String, AgentStatus)],
    queue: &mut Vec<String>,
    nudge: Option<&str>,
) -> Vec<LoopAction> {
    let mut actions = Vec::new();
    for (name, status) in states {
        match status {
            AgentStatus::Working => actions.push(LoopAction::Wait),
            AgentStatus::Blocked => actions.push(LoopAction::Blocked { name: name.clone() }),
            // parity with the durable loop (bd-unknown-skip-dispatch): never
            // dispatch to an unclassified agent — a prompt could land in a
            // half-dead session and be silently lost
            AgentStatus::Unknown => actions.push(LoopAction::Wait),
            AgentStatus::Idle | AgentStatus::Done => {
                if !queue.is_empty() {
                    let text = queue.remove(0); // FIFO: stages in declared order
                                                // bead/resume prompts carry a {AGENT_NAME} placeholder —
                                                // substitute the receiving agent's herdr name (same
                                                // contract as the durable loop's plan_actions)
                    actions.push(LoopAction::Send {
                        name: name.clone(),
                        text: text.replace("{AGENT_NAME}", name),
                    });
                } else if nudge.is_some() {
                    actions.push(LoopAction::Nudge { name: name.clone() });
                } else {
                    actions.push(LoopAction::Wait);
                }
            }
        }
    }
    actions
}

/// Build the instruction queue from --queue / --workflow.
/// Clamp the active cadence to the 5s floor (consistent with the workflow's
/// `INTERVAL_MIN` in `next_interval_secs`). Pure — the warning is emitted by
/// the caller when the value changes.
fn clamp_active_interval(v: u64) -> u64 {
    v.max(5)
}

fn build_queue(queue: Option<&str>, workflow: Option<&str>) -> Result<Vec<String>> {
    let mut items: Vec<String> = Vec::new();
    if let Some(wf) = workflow {
        let workflows = config::Workflows::load()?;
        let steps = workflows
            .workflows
            .get(wf)
            .ok_or_else(|| anyhow!("workflow not found: {wf} (flywheel workflows list)"))?;
        for step in steps {
            match step {
                WorkflowStep::Send { stage, msg, .. } => {
                    let text = match (stage.as_deref(), msg.as_deref()) {
                        (Some(s), _) => stage_prompt(s)?,
                        (None, Some(m)) => m.to_string(),
                        (None, None) => bail!("workflow {wf}: send step needs stage or msg"),
                    };
                    items.push(text);
                }
                // wait steps are implicit in the polling loop
                WorkflowStep::Wait { .. } => {}
            }
        }
    }
    if let Some(q) = queue {
        for token in q.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Ok(p) = stage_prompt(token) {
                items.push(p);
            } else {
                items.push(token.to_string());
            }
        }
    }
    Ok(items)
}

#[allow(clippy::too_many_arguments)] // clap command handler: one param per CLI flag.
fn cmd_loop(
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
    on_blocked: &str,
    to: &str,
    no_confirm: bool,
    confirm_timeout_ms: Option<u64>,
) -> Result<()> {
    if interval_secs < 5 {
        bail!("--interval must be >= 5 seconds");
    }
    // --interval-active clamps to the 5s floor (consistent with the
    // workflow's INTERVAL_MIN) rather than erroring — the operator asked for
    // responsiveness, not an error
    let active_secs = clamp_active_interval(active_interval_secs);
    if active_secs != active_interval_secs {
        eprintln!(
            "flywheel: warning: --interval-active {active_interval_secs}s below the 5s floor — clamping to 5"
        );
    }
    if !matches!(on_blocked, "continue" | "stop" | "mail") {
        bail!("--on-blocked must be continue|stop|mail");
    }
    let mut queue_items = build_queue(queue, workflow)?;

    // durable mode (default): run the loop as a Resonate workflow so it
    // survives crashes and restarts. --local / --once use the sync loop.
    if !local && !once {
        // durable loops have no wall clock; --timeout-ms becomes a poll cap
        // (deterministic across replays)
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
            confirm_sends: !no_confirm,
            confirm_timeout_ms,
            invocation: String::new(),
            unknown_diagnose_threshold: cfg.unknown_diagnose_threshold(),
            respawn_budget: cfg.respawn_budget(),
        };
        let rt = tokio::runtime::Runtime::new()?;
        println!(
            "loop: durable swarm monitor for {} (resume: flywheel loop {project} with the same flags)",
            dim(project)
        );
        let result = rt.block_on(durable::run_durable_loop(cfg, params))?;
        println!("loop: {result}");
        log(
            project,
            EventKind::Loop,
            json!({ "status": "complete", "result": result }),
        );
        return Ok(());
    }

    if queue_items.is_empty() && nudge.is_none() {
        eprintln!("flywheel: no instructions queued and no --nudge — loop will only monitor");
    }

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));

    log(
        project,
        EventKind::Loop,
        json!({
            "interval_secs": interval_secs,
            "queue_len": queue_items.len(),
            "to": to,
            "on_blocked": on_blocked,
        }),
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
        // re-check deadline
        if let Some(d) = deadline && std::time::Instant::now() >= d {
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
                .map(|a| (agent_target(a).to_string(), a.agent_status))
                .collect()
        } else {
            let resolved = resolve_agents(&mut c, &ws_id, project, to)?;
            resolved
                .iter()
                .map(|a| (agent_target(a).to_string(), a.agent_status))
                .collect()
        };

        let actions = plan_actions(&managed, &mut queue_items, nudge);
        // with on_blocked=stop, no work may be dispatched this iteration once a
        // blocker is known — otherwise sends before the blocked agent in the
        // action list already fired (order-dependent behavior)
        if on_blocked == "stop" {
            let blocked: Vec<&str> = actions
                .iter()
                .filter_map(|a| match a {
                    LoopAction::Blocked { name } => Some(name.as_str()),
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
                LoopAction::Send { name, text } => {
                    if let Err(e) = c.agent_prompt(name, text, None) {
                        eprintln!("flywheel: send to {name} failed: {e:#}");
                        // the item was already popped by plan_actions — put it
                        // back at the FRONT so the next poll retries it (a
                        // dead agent never silently consumes a prompt)
                        queue_items.insert(0, text.clone());
                        continue;
                    }
                    log(
                        project,
                        EventKind::Send,
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
                LoopAction::Nudge { name } => {
                    let n = nudge.unwrap_or("proceed");
                    if let Err(e) = c.agent_prompt(name, n, None) {
                        eprintln!("flywheel: nudge to {name} failed: {e:#}");
                    } else {
                        log(
                            project,
                            EventKind::Send,
                            json!({ "to": name, "nudge": true, "text_len": n.len() }),
                        );
                        println!("{} → {name} (nudge)", dim("[loop] sent"));
                        any_work = true;
                    }
                }
                LoopAction::Blocked { name } => {
                    any_blocked = true;
                    match on_blocked {
                        "mail" => {
                            if let Ok(m) = mail_or_die(cfg) {
                                let key = project_dir_str(cfg, project)?;
                                let _ = m.ensure_project(&key);
                                // the recipient is the agent's REGISTERED
                                // agent-mail identity (bd-odc) — the herdr
                                // name is not a registrable identity
                                let mail_target = resolve_mail_target(name);
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
                LoopAction::Wait => {}
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
                bail!("loop: agents blocked (on_blocked=continue; pass --on-blocked stop|mail to control)");
            }
            return Ok(());
        }

        if queue_drained && all_settled && !any_work {
            println!("{}", dim("[loop] queue drained, swarm settled"));
            log(project, EventKind::Loop, json!({ "status": "complete" }));
            return Ok(());
        }

        // wait for next poll (bounded by deadline)
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
mod loop_tests {
    use super::*;

    fn st(name: &str, s: AgentStatus) -> (String, AgentStatus) {
        (name.to_string(), s)
    }

    #[test]
    fn active_interval_clamps_to_floor() {
        assert_eq!(clamp_active_interval(3), 5);
        assert_eq!(clamp_active_interval(0), 5);
        assert_eq!(clamp_active_interval(5), 5);
        assert_eq!(clamp_active_interval(15), 15);
        assert_eq!(clamp_active_interval(30), 30);
    }

    #[test]
    fn idle_agent_pops_queue() {
        let states = vec![st("a", AgentStatus::Idle)];
        let mut q = vec!["p1".into(), "p6".into()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1);
        match &actions[0] {
            LoopAction::Send { name, text } => {
                assert_eq!(name, "a");
                assert_eq!(text, "p1");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn sync_loop_substitutes_agent_name_at_dispatch() {
        // the sync loop (--once/--local) dispatches the same bead prompts —
        // the {AGENT_NAME} placeholder must resolve to the receiving agent
        let states = vec![st("flywheel-proj-oc", AgentStatus::Idle)];
        let mut q = vec!["write to .flywheel/agent-state/{AGENT_NAME}.json".into()];
        let actions = plan_actions(&states, &mut q, None);
        match &actions[0] {
            LoopAction::Send { text, .. } => {
                assert!(text.contains(".flywheel/agent-state/flywheel-proj-oc.json"));
                assert!(!text.contains("{AGENT_NAME}"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
        // plain prompts stay untouched
        let states = vec![st("a", AgentStatus::Idle)];
        let mut q = vec!["t1".into()];
        let actions = plan_actions(&states, &mut q, None);
        match &actions[0] {
            LoopAction::Send { text, .. } => assert_eq!(text, "t1"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn queue_drains_across_idle_agents_in_order() {
        let states = vec![st("a", AgentStatus::Idle), st("b", AgentStatus::Idle)];
        let mut q = vec!["p1".into(), "p6".into()];
        let actions = plan_actions(&states, &mut q, None);
        assert!(q.is_empty());
        match (&actions[0], &actions[1]) {
            (LoopAction::Send { name: n1, text: t1 }, LoopAction::Send { name: n2, text: t2 }) => {
                assert_eq!((n1, t1), (&"a".to_string(), &"p1".to_string()));
                assert_eq!((n2, t2), (&"b".to_string(), &"p6".to_string()));
            }
            other => panic!("expected two sends, got {other:?}"),
        }
    }

    #[test]
    fn working_agent_gets_nothing() {
        let states = vec![st("a", AgentStatus::Working)];
        let mut q = vec!["p1".into()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1); // queue untouched
        assert!(matches!(actions[0], LoopAction::Wait));
    }

    #[test]
    fn blocked_agent_reports() {
        let states = vec![st("a", AgentStatus::Blocked)];
        let mut q = vec!["p1".into()];
        let actions = plan_actions(&states, &mut q, None);
        assert!(matches!(&actions[0], LoopAction::Blocked { name } if name == "a"));
        assert_eq!(q.len(), 1); // blocked agent doesn't consume the queue
    }

    #[test]
    fn unknown_agent_keeps_queue() {
        // parity with the durable loop (bd-unknown-skip-dispatch): herdr
        // cannot classify an unknown agent confidently — a prompt could land
        // in a half-dead session and be silently lost. Never dispatch, never
        // nudge; the queue stays intact.
        let states = vec![st("a", AgentStatus::Unknown)];
        let mut q = vec!["p1".into()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1);
        assert!(matches!(actions[0], LoopAction::Wait));
        let mut q2: Vec<String> = vec![];
        let actions = plan_actions(&states, &mut q2, Some("proceed"));
        assert!(
            matches!(actions[0], LoopAction::Wait),
            "no nudge to unknown"
        );
    }

    #[test]
    fn empty_queue_with_nudge_nudges() {
        let states = vec![st("a", AgentStatus::Idle)];
        let mut q: Vec<String> = vec![];
        let actions = plan_actions(&states, &mut q, Some("proceed"));
        assert!(matches!(&actions[0], LoopAction::Nudge { name } if name == "a"));
    }

    #[test]
    fn empty_queue_no_nudge_waits() {
        let states = vec![st("a", AgentStatus::Done)];
        let mut q: Vec<String> = vec![];
        let actions = plan_actions(&states, &mut q, None);
        assert!(matches!(actions[0], LoopAction::Wait));
    }

    #[test]
    fn done_treated_like_idle() {
        let states = vec![st("a", AgentStatus::Done)];
        let mut q = vec!["p8".into()];
        let actions = plan_actions(&states, &mut q, None);
        assert!(q.is_empty());
        assert!(matches!(&actions[0], LoopAction::Send { text, .. } if text == "p8"));
    }

    #[test]
    fn name_matching_handles_numbered_agents() {
        let base = "flywheel-proj-";
        assert!(name_matches_want(
            &format!("{base}oc"),
            &format!("{base}oc")
        ));
        assert!(name_matches_want(
            &format!("{base}oc2"),
            &format!("{base}oc")
        ));
        assert!(name_matches_want(
            &format!("{base}oc10"),
            &format!("{base}oc")
        ));
        // prefix collision: ocr must NOT match oc
        assert!(!name_matches_want(
            &format!("{base}ocr"),
            &format!("{base}oc")
        ));
        assert!(!name_matches_want(
            &format!("{base}ocx"),
            &format!("{base}oc")
        ));
    }

    #[test]
    fn agent_naming_scheme() {
        assert_eq!(agent_name_n("proj", "oc", 1), "flywheel-proj-oc");
        assert_eq!(agent_name_n("proj", "oc", 2), "flywheel-proj-oc2");
        assert_eq!(agent_name_n("a/b/proj", "pi", 3), "flywheel-proj-pi3");
        assert_eq!(agent_ordinal("flywheel-proj-oc", "proj", "oc"), Some(1));
        assert_eq!(agent_ordinal("flywheel-proj-oc2", "proj", "oc"), Some(2));
        assert_eq!(agent_ordinal("flywheel-proj-ocr", "proj", "oc"), None);
        assert_eq!(agent_ordinal("flywheel-proj-gk", "proj", "oc"), None);
    }

    #[test]
    fn watch_matches_event_by_pane_id_not_agent_kind() {
        // the pane.agent_status_changed push carries the agent KIND
        // ("opencode"), never the instance name — matching must go through
        // the pane id, which IS in the payload
        let targets = vec![
            ("w1:p1".to_string(), "flywheel-x-oc".to_string()),
            ("w1:p2".to_string(), "flywheel-x-pi".to_string()),
        ];
        assert_eq!(
            watch_name_for_pane(&targets, "w1:p1"),
            Some("flywheel-x-oc")
        );
        assert_eq!(
            watch_name_for_pane(&targets, "w1:p2"),
            Some("flywheel-x-pi")
        );
        assert_eq!(watch_name_for_pane(&targets, "w9:p9"), None);
        // pane ids never match by kind: the event's `agent` field is NOT a
        // lookup key (that was the bug — kind vs name confusion)
        assert_eq!(watch_name_for_pane(&targets, "opencode"), None);
    }

    #[test]
    fn watch_matches_nameless_agents_by_pane() {
        // a nameless pi agent's display name IS its pane id
        let targets = vec![("w1:p2".to_string(), "w1:p2".to_string())];
        assert_eq!(watch_name_for_pane(&targets, "w1:p2"), Some("w1:p2"));
    }
}

// ---- safety / guards / policy ----

fn cmd_safety(cfg: &Config) -> Result<()> {
    let policy = config::Policy::load()?;
    println!("active guards: {}", policy.deny.len());
    for p in &policy.deny {
        println!("  {}", dim(p));
    }
    let mut c = match Client::connect(Some(&cfg.socket_path())) {
        Ok(c) => c,
        Err(_) => {
            println!("herdr: {}", error("unreachable"));
            return Ok(());
        }
    };
    println!("herdr: {}", status_paint(AgentStatus::Idle));
    if c.ping().is_ok() {
        println!("herdr socket: ok");
    }
    println!(
        "br: {}",
        if command_exists("br") {
            status_paint(AgentStatus::Idle)
        } else {
            error("missing")
        }
    );
    println!(
        "bv: {}",
        if command_exists("bv") {
            status_paint(AgentStatus::Idle)
        } else {
            error("missing")
        }
    );
    Ok(())
}

fn cmd_guards(_cfg: &Config) -> Result<()> {
    let policy = config::Policy::load()?;
    if policy.deny.is_empty() {
        println!("no guards configured (flywheel policy add \"rm -rf\")");
        return Ok(());
    }
    for p in &policy.deny {
        println!("{p}");
    }
    Ok(())
}

fn cmd_policy(_cfg: &Config, cmd: &PolicyCmd) -> Result<()> {
    let path = config::config_dir().join("policy.toml");
    let mut policy = config::Policy::load()?;
    match cmd {
        PolicyCmd::List => {
            for (i, p) in policy.deny.iter().enumerate() {
                println!("[{i}] {p}");
            }
            if policy.deny.is_empty() {
                println!("no deny patterns");
            }
        }
        PolicyCmd::Add { pattern } => {
            policy.deny.push(pattern.clone());
            save_policy(&path, &policy)?;
            println!("guard added: {pattern}");
        }
        PolicyCmd::Remove { index } => {
            if *index >= policy.deny.len() {
                bail!("no guard at index {index}");
            }
            let removed = policy.deny.remove(*index);
            save_policy(&path, &policy)?;
            println!("guard removed: {removed}");
        }
        PolicyCmd::Path => {
            println!("{}", path.display());
        }
    }
    Ok(())
}

fn save_policy(path: &std::path::Path, policy: &config::Policy) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(policy)?;
    std::fs::write(path, raw)?;
    Ok(())
}

// ---- deps / config / upgrade / tutorial ----

fn resonate_health(cfg: &Config) -> Result<()> {
    let url = cfg
        .resonate_url()
        .unwrap_or_else(|| "http://localhost:8001".to_string());
    let resp = ureq::get(&format!("{url}/health"))
        .call()
        .map_err(|e| anyhow!("resonate server unreachable at {url}: {e}"))?;
    if !(200..300).contains(&resp.status()) {
        bail!("resonate server unhealthy at {url}");
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Per-kind notes for the beads-agent integration template. Keep terse —
/// the durable-loop behavior is kind-agnostic; these cover session/rendering
/// quirks that agents should know about themselves.
fn kind_notes(kind: &str) -> Option<&'static str> {
    match kind {
        "opencode" => Some(
            "- Config: ~/.config/opencode (sessions in opencode's store, ~/.local/share/opencode).\n- Terminal rendering uses the alternate screen: your context state can leave host\n  scrollback, so the state file is the reliable context channel — write it at ~80%.\n- herdr confirms landings fast (~0.6s).",
        ),
        "pi" => Some(
            "- Config + sessions: ~/.pi (session files under ~/.pi/agent/sessions/).\n- Your agent name is applied automatically at spawn; use it in `--actor` and the\n  state-file path. Landings confirm fast (~0.6s).",
        ),
        "grok" => Some(
            "- Config + sessions: ~/.grok (active sessions in ~/.grok/active_sessions.json).\n- Landing latency can be higher: the orchestrator's confirm wait covers it — you\n  only need to reach working/blocked, not finish, for the send to count as landed.",
        ),
        _ => None,
    }
}

/// The shared beads-agent BODY — the lifecycle, coordination rules, and
/// context contract that all three agent kinds follow. Per-kind frontmatter
/// and notes live in the wrappers below. Edit the templates in
/// `integrations/` and rebuild — re-run `flywheel integration <kind>` after
/// a rebuild to update the installed subagent files.
const INTEGRATION_BODY: &str = include_str!("../integrations/beads-body.md.tpl");

/// Per-kind subagent wrappers: frontmatter (tool-specific format) + {{BODY}}.
fn integration_wrapper(kind: &str) -> Option<&'static str> {
    match kind {
        "opencode" => Some(include_str!("../integrations/opencode-beads.md.tpl")),
        "pi" => Some(include_str!("../integrations/pi-beads.md.tpl")),
        "grok" => Some(include_str!("../integrations/grok-beads.md.tpl")),
        _ => None,
    }
}

/// Render the beads SUBAGENT definition for one agent kind. Pure: `version`
/// and `generated` are injected by the caller (deterministic tests).
fn render_integration(kind: &str, version: &str, generated: &str) -> Result<String> {
    let wrapper = integration_wrapper(kind)
        .ok_or_else(|| anyhow!("unknown agent kind: {kind} (known: opencode, pi, grok)"))?;
    let notes = kind_notes(kind).unwrap_or_default();
    let body = INTEGRATION_BODY
        .replace("{{KIND_NOTES}}", notes)
        .replace("{{VERSION}}", version)
        .replace("{{GENERATED}}", generated);
    Ok(wrapper
        .replace("{{BODY}}", &body)
        .replace("{{VERSION}}", version)
        .replace("{{GENERATED}}", generated))
}

/// `flywheel integration [KIND] [--out PATH]` — generate the beads-agent
/// integration instructions for opencode/pi/grok agents. With no KIND, list
/// the available kinds. With `--out`, write the rendered file instead of
/// stdout (e.g. append it to a project's AGENTS.md).
fn cmd_integration(kind: Option<&str>, out: Option<&Path>) -> Result<()> {
    let Some(kind) = kind else {
        println!("flywheel beads-agent subagents (flywheel {VERSION}):");
        for (k, path) in [
            ("opencode", "~/.config/opencode/agents/beads.md"),
            ("pi", "~/.pi/agent/agents/beads.md"),
            ("grok", "~/.grok/agents/beads.md"),
        ] {
            println!("  {k:9} flywheel integration {k} --out {path}");
        }
        println!(
            "templates: integrations/opencode-beads.md.tpl, pi-beads.md.tpl, grok-beads.md.tpl + beads-body.md.tpl — edit, rebuild, re-run to update"
        );
        return Ok(());
    };
    // accept the spawn shorts (oc/pi/gk) as well as the full kind names
    let kind = match kind {
        "oc" => "opencode",
        "gk" => "grok",
        other => other,
    };
    let rendered = render_integration(kind, VERSION, &state::now_iso())?;
    match out {
        Some(path) => {
            std::fs::write(path, &rendered)?;
            println!(
                "wrote {} ({} bytes)",
                dim(&path.display().to_string()),
                rendered.len()
            );
        }
        None => print!("{rendered}"),
    }
    Ok(())
}

/// Auto-wire the toron MCP server into the agent MCP configs.
///
/// Idempotent upsert per agent kind:
/// - opencode  ~/.config/opencode/opencode.json   `mcp.toron`   (JSON, remote)
/// - pi        ~/.pi/agent/mcp.json               `mcpServers.toron` (JSON)
/// - grok      ~/.grok/config.toml                `[mcp_servers.toron]` (TOML)
///
/// Never clobbers other MCP servers; only the `toron` key is added/updated.
/// `dry_run` prints the target change without touching any file.
/// `boot_port` (>0) also spawns `toron serve --mcp-http <port>` on a
/// background task (requires the toron binary on PATH + operator-key env).
fn toron_setup_mcp(
    url: Option<&str>,
    agents: &str,
    boot_port: u16,
    dry_run: bool,
    flywheel: bool,
) -> Result<()> {
    let url = url.unwrap_or("http://127.0.0.1:18080/mcp");
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME unset"))?;
    let home = std::path::Path::new(&home);
    let mut changed: Vec<String> = Vec::new();

    for kind in agents.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let kind = match kind {
            "oc" => "opencode",
            "gk" => "grok",
            other => other,
        };
        match kind {
            "opencode" => {
                let p = home.join(".config/opencode/opencode.json");
                changed.push(upsert_opencode_toron(&p, url, dry_run)?);
            }
            "pi" => {
                let p = home.join(".pi/agent/mcp.json");
                changed.push(upsert_pi_toron(&p, url, dry_run)?);
            }
            "grok" => {
                let p = home.join(".grok/config.toml");
                changed.push(upsert_grok_toron(&p, url, dry_run)?);
            }
            other => {
                eprintln!("flywheel: unknown agent kind '{other}' — use opencode,pi,grok (shorts oc,pi,gk)");
            }
        }
    }

    if flywheel {
        // also point flywheel's OWN mail backend at the same toron URL so
        // flywheel + all agents share one mailbox.
        changed.push(setup_flywheel_backend(url, dry_run)?);
    }

    if changed
        .iter()
        .any(|c| c.contains("added") || c.contains("updated"))
    {
        println!("toron MCP wired: {url}  {}", changed.join(" · "));
        println!("restart the agent editor for a new session to pick up the MCP server.");
        if flywheel {
            println!(
                "flywheel backend: next session routes mail through {url} (env FLYWHEEL_AGENTMAIL_URL overrides config)."
            );
        }
    } else {
        println!("toron MCP already present at {url} — nothing to change.");
    }

    if boot_port > 0 && !dry_run {
        boot_toron_server(boot_port)?;
    } else if boot_port > 0 {
        println!("(--dry-run) would boot toron serve --mcp-http 127.0.0.1:{boot_port}");
    }
    Ok(())
}

/// opencode: `mcp.toron` in top-level JSON config.
fn upsert_opencode_toron(path: &std::path::Path, url: &str, dry: bool) -> Result<String> {
    let mut v: serde_json::Value = load_json_or_default(path)?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("opencode config root must be an object"))?;
    // Ensure `mcp` exists *in its own statement* so the borrow of `obj` ends,
    // then re-take a single mutable borrow. Avoids the double-borrow E0500.
    if !obj.contains_key("mcp") || !obj["mcp"].is_object() {
        obj.insert("mcp".into(), serde_json::json!({}));
    }
    let mcp = obj["mcp"].as_object_mut().unwrap();
    let prev = mcp.get("toron").cloned();
    let want = serde_json::json!({"type":"remote","url":url,"enabled":true});
    let action = if prev.is_some() {
        if prev.as_ref() == Some(&want) {
            return Ok("opencode unchanged".to_string());
        }
        "updated"
    } else {
        "added"
    };
    mcp.insert("toron".into(), want);
    if dry {
        return Ok(format!("opencode {action} ({})", pretty_path(path)));
    }
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(format!("opencode {action}"))
}

/// pi: `mcpServers.toron` in JSON.
fn upsert_pi_toron(path: &std::path::Path, url: &str, dry: bool) -> Result<String> {
    let mut v: serde_json::Value = load_json_or_default(path)?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("pi mcp.json root must be an object"))?;
    if !obj.contains_key("mcpServers") || !obj["mcpServers"].is_object() {
        obj.insert("mcpServers".into(), serde_json::json!({}));
    }
    let servers = obj["mcpServers"].as_object_mut().unwrap();
    let prev = servers.get("toron").cloned();
    let want = serde_json::json!({"url":url,"directTools":true});
    let action = if prev.is_some() {
        if prev.as_ref() == Some(&want) {
            return Ok("pi unchanged".to_string());
        }
        "updated"
    } else {
        "added"
    };
    servers.insert("toron".into(), want);
    if dry {
        return Ok(format!("pi {action} ({})", pretty_path(path)));
    }
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(format!("pi {action}"))
}

/// grok: `[mcp_servers.toron]` section in TOML. Line-based idempotent upsert
/// (no toml dependency): if an `[mcp_servers.toron]` header exists, rewrite the
/// following url/enabled lines; else append a new block.
fn upsert_grok_toron(path: &std::path::Path, url: &str, dry: bool) -> Result<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // find `[mcp_servers.toron]` (exact or any `[mcp_servers.toron*]` is != ours,
    // so match the bare header only).
    let mut idx: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim() == "[mcp_servers.toron]" {
            idx = Some(i);
            break;
        }
    }
    match idx {
        Some(i) => {
            // already present: replace the section body's url/enabled lines
            // up to the next `[section]` header.
            let mut j = i + 1;
            let mut found_url = false;
            while j < lines.len() && !lines[j].trim_start().starts_with('[') {
                let t = lines[j].trim();
                if t.starts_with("url =") {
                    lines[j] = format!("url = \"{url}\"");
                    found_url = true;
                }
                j += 1;
            }
            if !found_url {
                // ensure an url line right after the header, before the next header
                lines.insert(i + 1, format!("url = \"{url}\""));
            }
            let act = if found_url { "updated" } else { "added url" };
            if dry {
                return Ok(format!("grok {act} ({})", pretty_path(path)));
            }
            std::fs::write(path, lines.join("\n") + "\n")?;
            Ok(format!("grok {act}"))
        }
        None => {
            if dry {
                return Ok(format!("grok added ({})", pretty_path(path)));
            }
            lines.push(String::from("[mcp_servers.toron]"));
            lines.push(format!("url = \"{url}\""));
            lines.push(String::from("enabled = true"));
            lines.push(String::new());
            std::fs::write(path, lines.join("\n") + "\n")?;
            Ok("grok added".to_string())
        }
    }
}

fn load_json_or_default(path: &std::path::Path) -> Result<serde_json::Value> {
    if path.exists() {
        let s = std::fs::read_to_string(path)?;
        if s.trim().is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    } else {
        Ok(serde_json::json!({}))
    }
}

fn pretty_path(p: &std::path::Path) -> String {
    if let Ok(h) = std::env::var("HOME") && let Ok(strip) = p.strip_prefix(&h) {
        return format!("~/{}", strip.display());
    }
    p.display().to_string()
}

/// Spawn `toron serve --mcp-http <port>` as a detached background daemon.
/// Requires the toron binary on PATH; propagates TORON_OPERATOR_KEY +
/// TORON_KEY_PASSPHRASE from the current env.
fn boot_toron_server(port: u16) -> Result<()> {
    let bind = format!("127.0.0.1:{port}");
    // refuse if a toron is already listening
    if std::net::TcpStream::connect(bind.as_str().parse::<std::net::SocketAddr>()?).is_ok() {
        println!("toron MCP server already listening at {bind} — skipping boot.");
        return Ok(());
    }
    let mut cmd = std::process::Command::new("toron");
    cmd.args(["serve", "--mcp-http", &bind, "--no-color"]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // preserve operator-key env so the full privileged surface works
    if std::env::var_os("TORON_OPERATOR_KEY").is_some() {
        // 193.1: a non-UTF-8 key is an actionable error (never a panic —
        // var().unwrap() would panic on a non-UTF-8 value).
        let key = std::env::var("TORON_OPERATOR_KEY")
            .with_context(|| "TORON_OPERATOR_KEY must be valid UTF-8 when set")?;
        cmd.env("TORON_OPERATOR_KEY", key);
    }
    cmd.spawn()?;
    println!("booted toron serve --mcp-http {bind} (detached).");
    Ok(())
}

/// Point flywheel's OWN mail backend at the toron URL.
///
/// Writes/updates the `[mail.toron] url` line in the flywheel config.toml so
/// flywheel and all registered agents converge on ONE toron mailbox (the
/// shared `[agent_mail]` section keeps the identity name). The env
/// `FLYWHEEL_AGENTMAIL_URL` still wins at load time (config.rs merges env over
/// the file) — config.toml is the durable default, env is the per-session
/// override. Idempotent; only touches the `[mail.toron]` block's `url`.
fn setup_flywheel_backend(url: &str, dry: bool) -> Result<String> {
    let path = config::config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    // Locate `[mail.toron]` and rewrite its `url = ...` line; if no section
    // exists, append one.
    let mut in_mail = false;
    let mut url_line: Option<usize> = None;
    let mut section_idx: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t.starts_with('[') && t.ends_with(']') && t != "[mail.toron]" {
            break; // past the mail.toron block — stop scanning
        }
        if t == "[mail.toron]" {
            in_mail = true;
            section_idx = Some(i);
        }
        if in_mail && t.starts_with("url =") {
            url_line = Some(i);
        }
    }

    let action = match url_line {
        Some(i) => {
            let existing = lines[i].trim();
            if existing == format!("url = \"{url}\"") {
                return Ok("flywheel unchanged".to_string());
            }
            lines[i] = format!("url = \"{url}\"");
            "updated".to_string()
        }
        None => match section_idx {
            Some(i) => {
                // mail.toron section present but no url line — insert right after
                // the section header.
                lines.insert(i + 1, format!("url = \"{url}\""));
                "added url".to_string()
            }
            None => {
                // no [mail.toron] at all — append fresh blocks (identity first).
                lines.push(String::from("[agent_mail]"));
                lines.push(String::from("name = \"flywheel\""));
                lines.push(String::new());
                lines.push(String::from("[mail.toron]"));
                lines.push(format!("url = \"{url}\""));
                lines.push(String::new());
                "added section".to_string()
            }
        },
    };

    if dry {
        return Ok(format!("flywheel backend {action}"));
    }
    std::fs::write(&path, lines.join("\n") + "\n")?;
    Ok(format!("flywheel backend {action}"))
}

#[cfg(test)]
mod protocol_check_tests {
    use super::*;

    /// (live, pin) -> severity. herdr's protocol is additive: higher is safe,
    /// lower is the real safety net.
    #[test]
    fn protocol_check_three_states() {
        for (live, pin, want) in [
            (19u64, 19u64, ProtocolCheck::Match),
            (20, 19, ProtocolCheck::Newer),
            (18, 19, ProtocolCheck::Older),
            (42, 19, ProtocolCheck::Newer),
            (1, 19, ProtocolCheck::Older),
            (0, 19, ProtocolCheck::Older),
            (19, 18, ProtocolCheck::Newer),
        ] {
            assert_eq!(protocol_check(live, pin), want, "live={live} pin={pin}");
        }
    }
}

/// Severity of a live herdr protocol number vs the verified pin.
///
/// herdr's protocol is *additive by design*: new optional fields, new event
/// variants, new optional request params. A live protocol HIGHER than the pin
/// is safe (the client ignores what it doesn't know) — `HDR_PROTOCOL` is the
/// *verified floor*, not the ceiling. A live protocol LOWER than the pin is
/// unsafe: the server may not understand our requests. That asymmetry is
/// what `protocol_check` encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolCheck {
    /// live == pin: exactly the verified contract.
    Match,
    /// live > pin: forward-compatible; the client may not use new features.
    Newer,
    /// live < pin: server too old; our requests may be rejected. Safety net.
    Older,
}

fn protocol_check(live: u64, pin: u64) -> ProtocolCheck {
    if live == pin {
        ProtocolCheck::Match
    } else if live > pin {
        ProtocolCheck::Newer
    } else {
        ProtocolCheck::Older
    }
}

/// The agent-mail identity the orchestrator acts as: the configured
/// `[agent_mail] name`, defaulting to "flywheel" — identical to
/// `Mail::from_config`'s default, so lease renewal and the `flywheel mail
/// locks` reservations always use the SAME identity.
fn orchestrator_mail_name(cfg: &Config) -> String {
    cfg.agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "flywheel".to_string())
}

#[cfg(test)]
mod mail_name_tests {
    use super::*;
    use config::MailConfig;

    #[test]
    fn configured_name_wins() {
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://x".into()),
                token: None,
                name: Some("Orchid".into()),
            }),
            ..Default::default()
        };
        assert_eq!(orchestrator_mail_name(&cfg), "Orchid");
    }

    #[test]
    fn default_name_is_flywheel() {
        // no agent_mail section at all
        assert_eq!(orchestrator_mail_name(&Config::default()), "flywheel");
        // agent_mail section without a name
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://x".into()),
                token: None,
                name: None,
            }),
            ..Default::default()
        };
        assert_eq!(orchestrator_mail_name(&cfg), "flywheel");
    }
}

fn cmd_deps(cfg: &Config) -> Result<()> {
    let mut ok = true;
    println!("flywheel {}", dim(VERSION));

    match Client::connect(Some(&cfg.socket_path())) {
        Ok(mut c) => match c.ping() {
            Ok(_) => println!("herdr socket: {}", status_paint(AgentStatus::Idle)),
            Err(e) => {
                ok = false;
                println!("herdr socket: {} ({e:#})", error("FAIL"));
            }
        },
        Err(e) => {
            ok = false;
            println!("herdr socket: {} ({e:#})", error("FAIL"));
        }
    }

    // herdr protocol drift check. The socket has no protocol method in v18,
    // so we shell out to the herdr CLI itself (`api schema --json` — the
    // canonical source; same binary as the socket server, so CLI vs socket
    // drift is not a concern). HDR_PROTOCOL is the *verified floor*: live ==
    // pin is green, live > pin is forward-compatible INFO (herdr is additive
    // by design — this is the signal that triggers a bd-protocol-v2X-bump
    // bead later), live < pin is ERROR + exit 1 (server too old — the safety
    // net, per ADR-0001 gap A1).
    match std::process::Command::new("herdr")
        .args(["api", "schema", "--json"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| herdr_shim::parse_protocol(&String::from_utf8_lossy(&o.stdout)))
    {
        Some(p) => match protocol_check(p, herdr_shim::HDR_PROTOCOL) {
            ProtocolCheck::Match => println!(
                "herdr protocol: {} (verified {})",
                paint_ok(&p.to_string()),
                herdr_shim::HDR_PROTOCOL
            ),
            ProtocolCheck::Newer => {
                println!(
                    "herdr protocol: {} (verified {})",
                    crate::output::warn(&p.to_string()),
                    herdr_shim::HDR_PROTOCOL
                );
                eprintln!(
                    "flywheel: INFO herdr protocol {p} > verified {} — forward-compatible (client ignores what it doesn't know), but flywheel may not use new features; rebuild against the newer schema when ready",
                    herdr_shim::HDR_PROTOCOL
                );
            }
            ProtocolCheck::Older => {
                ok = false;
                println!(
                    "herdr protocol: {} (verified {})",
                    crate::output::error(&p.to_string()),
                    herdr_shim::HDR_PROTOCOL
                );
                eprintln!(
                    "flywheel: ERROR herdr protocol {p} < verified {} — server too old, our requests may be rejected; upgrade herdr or rebuild flywheel against the pinned schema",
                    herdr_shim::HDR_PROTOCOL
                );
            }
        },
        None => {
            println!(
                "herdr protocol: {} (verified {})",
                crate::output::warn("unknown"),
                herdr_shim::HDR_PROTOCOL
            );
            eprintln!(
                "flywheel: WARNING cannot read herdr protocol (herdr api schema --json failed)"
            );
        }
    }

    for tool in ["br", "bv", "herdr"] {
        if command_exists(tool) {
            println!("{tool}: {}", status_paint(AgentStatus::Idle));
        } else {
            ok = false;
            println!("{tool}: {}", error("missing"));
        }
    }

    match resonate_health(cfg) {
        Ok(_) => println!("resonate server: {}", status_paint(AgentStatus::Idle)),
        Err(e) => {
            ok = false;
            println!("resonate server: {} ({e:#})", error("FAIL"));
        }
    }

    match mail_shim::Mail::from_config(Some(&cfg.toron_mail_config())) {
        Ok(Some(m)) => match m.health() {
            Ok(_) => println!("agent-mail: {}", status_paint(AgentStatus::Idle)),
            Err(e) => {
                ok = false;
                println!("agent-mail: {} ({e:#})", error("FAIL"));
            }
        },
        Ok(None) => println!("agent-mail: {}", dim("not configured (set [mail.toron])")),
        Err(e) => {
            ok = false;
            println!("agent-mail: {} ({e:#})", error("FAIL"));
        }
    }

    println!("config: {}", config::config_file().display());
    if !ok {
        bail!("deps check failed");
    }
    Ok(())
}

fn cmd_config(cfg: &Config, cmd: &ConfigCmd) -> Result<()> {
    let path = config::config_file();
    match cmd {
        ConfigCmd::List => {
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            println!("{raw}");
            println!("# effective (flags/env > file):");
            println!("socket_path = {:?}", cfg.socket_path());
            println!(
                "workspace_root = {:?}",
                cfg.workspace_root().display().to_string()
            );
            println!("default_timeout_ms = {}", cfg.timeout_ms());
        }
        ConfigCmd::Get { key } => {
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            let cfg: config::Config = toml::from_str(&raw).unwrap_or_default();
            let value = match key.as_str() {
                "socket_path" => cfg.socket_path,
                "workspace_root" => cfg.workspace_root,
                "default_timeout_ms" => cfg.default_timeout_ms.map(|v| v.to_string()),
                _ => bail!("unknown key: {key} (socket_path|workspace_root|default_timeout_ms)"),
            };
            println!("{}", value.unwrap_or_default());
        }
        ConfigCmd::Set { key, value } => {
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            let mut cfg: config::Config = toml::from_str(&raw).unwrap_or_default();
            match key.as_str() {
                "socket_path" => cfg.socket_path = Some(value.clone()),
                "workspace_root" => cfg.workspace_root = Some(value.clone()),
                "default_timeout_ms" => cfg.default_timeout_ms = Some(value.parse()?),
                "backend.multiplexer" => {
                    cfg.backend.get_or_insert_with(Default::default).multiplexer = value.clone()
                }
                "backend.mail" => {
                    cfg.backend.get_or_insert_with(Default::default).mail = value.clone()
                }
                _ => bail!(
                    "unknown key: {key} (socket_path|workspace_root|default_timeout_ms|backend.multiplexer|backend.mail)"
                ),
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
            println!("set {key} = {value}");
        }
        ConfigCmd::Path => println!("{}", path.display()),
    }
    Ok(())
}

/// `flywheel backend` — discovery + selection of the pluggable backends
/// (ADR-0002 R0.10). The `list` subcommand enumerates compiled-in backends
/// (name/kind/ready|stub/active/compiled), `status` shows the active pair +
/// config source + health, and `default` persists a backend to config.toml
/// `[backend]` (env/flag still override at runtime).
///
/// governed-by: ADR-0002 (bd-cli-backend-subcommand)
fn cmd_backend(cfg: &Config, cmd: &BackendCmd) -> Result<()> {
    match cmd {
        BackendCmd::List => {
            let active_mux = backend::resolve_backend_name(cfg);
            let active_mail = backend::resolve_mail_name(cfg);
            let compiled = backend::compiled_backends();
            let mut rows: Vec<Value> = Vec::new();
            // multiplexers
            for (name, stub) in [("herdr", false), ("superlogical", true), ("tmux", true)] {
                rows.push(json!({
                    "name": name,
                    "kind": "multiplexer",
                    "status": if stub { "stub" } else { "ready" },
                    "active": active_mux == name,
                    "compiled_feature": compiled.contains(&name),
                }));
            }
            // mail: agentmail = real agent-mail backend; toron = toron facade
            for (name, stub) in [("agentmail", false), ("toron", false)] {
                rows.push(json!({
                    "name": name,
                    "kind": "mail",
                    "status": if stub { "stub" } else { "ready" },
                    "active": active_mail == name,
                    "compiled_feature": compiled.contains(&name),
                }));
            }
            print_json(&json!({"backends": rows}));
        }
        BackendCmd::Status => {
            let mux = backend::resolve_backend_name(cfg);
            let mail = backend::resolve_mail_name(cfg);
            // config source: env overrides config field over default. Report
            // which layer selected the active name (env/.env/config/default).
            let mux_source = if std::env::var_os("FLYWHEEL_BACKEND").is_some_and(|v| !v.is_empty())
            {
                "env"
            } else if cfg
                .backend
                .as_ref()
                .map(|b| b.multiplexer.as_str())
                .is_some_and(|b| b == mux)
            {
                "config"
            } else {
                "default"
            };
            let mail_source = if std::env::var_os("FLYWHEEL_MAIL").is_some_and(|v| !v.is_empty()) {
                "env"
            } else if cfg
                .backend
                .as_ref()
                .map(|b| b.mail.as_str())
                .is_some_and(|b| b == mail)
            {
                "config"
            } else {
                "default"
            };
            // health: multiplexer = socket reachable; mail = the selected
            // backend's health() (agentmail → AM health_check; toron → toron
            // facade health; not-compiled → false).
            let mux_health = backend::multiplexer_backend(cfg)
                .map(|b| b.agent_list().is_ok())
                .unwrap_or(false);
            let mail_health = backend::mail_backend(cfg)
                .and_then(|m| {
                    backend::run_mail(m.health())
                        .map(|_| true)
                        .map_err(Into::into)
                })
                .unwrap_or(false);
            print_json(&json!({
                "multiplexer": {"name": mux, "source": mux_source, "healthy": mux_health},
                "mail": {"name": mail, "source": mail_source, "healthy": mail_health},
            }));
        }
        BackendCmd::Default { kind, name } => {
            let path = config::config_file();
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            let mut cfg_w: config::Config = toml::from_str(&raw).unwrap_or_default();
            let bc = cfg_w.backend.get_or_insert_with(Default::default);
            match kind.as_str() {
                "multiplexer" => bc.multiplexer = name.clone(),
                "mail" => bc.mail = name.clone(),
                other => bail!("unknown backend kind: {other} (multiplexer|mail)"),
            }
            // A `BackendConfig::default()` has empty fields; when the user sets
            // one kind, initialize the OTHER to the currently-resolved default
            // so the written config never contains empty strings (which would
            // deserialize as unset — harmless, but sloppy).
            if bc.multiplexer.is_empty() {
                bc.multiplexer = backend::resolve_backend_name(cfg);
            }
            if bc.mail.is_empty() {
                bc.mail = backend::resolve_mail_name(cfg);
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, toml::to_string_pretty(&cfg_w)?)?;
            println!("default {kind} = {name}");
        }
    }
    Ok(())
}

/// `flywheel profile` — the agent profile catalog (ADR-0005 Spec 1/1b).
///
/// `list` shows compiled-in defaults + local `profiles.toml` entries; `show`
/// prints the full profile (behavior preamble + skills); `create`/`edit`
/// write `~/.config/flywheel/profiles.toml`; `run @name` resolves the profile
/// (host scope, operator identity) and spawns the agent via the configured
/// multiplexer backend, optionally invoking a workflow after spawn.
///
/// governed-by: ADR-0005 (bd-p2-cli-wn8)
fn cmd_profile(cfg: &Config, cmd: &ProfileCmd) -> Result<()> {
    use profile::{ProfileCatalog, Profile};

    match cmd {
        ProfileCmd::List => {
            let cat = ProfileCatalog::load()?;
            // stdout = data; --json available to agent drivers via robot-style
            let mut rows: Vec<Value> = Vec::new();
            for name in cat.names() {
                let p = cat.get(&name).expect("names from catalog");
                rows.push(json!({
                    "name": name,
                    "harness": p.harness,
                    "model": p.model,
                    "role": p.role,
                    "backend": cat.backend(&name),
                    "default_workflow": p.default_workflow,
                    "builtin": p.skills.is_empty() && p.behavior.is_empty(),
                }));
            }
            print_json(&json!({ "profiles": rows }));
        }
        ProfileCmd::Show { name } => {
            let cat = ProfileCatalog::load()?;
            let p = cat
                .get(name)
                .ok_or_else(|| anyhow!("unknown profile '@{name}' (flywheel profile list)"))?;
            print_json(&json!({
                "name": name,
                "harness": p.harness,
                "model": p.model,
                "role": p.role,
                "backend": cat.backend(name),
                "default_workflow": p.default_workflow,
                "behavior": p.behavior,
                "task_hint": p.task_hint,
                "skills": p.skills,
            }));
        }
        ProfileCmd::Create { name, harness, model, role, behavior, task_hint, backend, default_workflow }
        | ProfileCmd::Edit { name, harness, model, role, behavior, task_hint, backend, default_workflow } => {
            let path = config::config_dir().join("profiles.toml");
            let cat = ProfileCatalog::load()?;
            let base = cat.get(name).cloned().unwrap_or_default();
            let merged = Profile {
                harness: harness.clone().unwrap_or(base.harness),
                model: model.clone().unwrap_or(base.model),
                role: role.clone().unwrap_or(base.role),
                behavior: behavior.clone().unwrap_or(base.behavior),
                task_hint: task_hint.clone().unwrap_or(base.task_hint),
                backend: backend.clone().unwrap_or(base.backend),
                default_workflow: default_workflow.clone().unwrap_or(base.default_workflow),
                skills: base.skills,
            };
            if merged.harness.is_empty() || merged.model.is_empty() {
                bail!("profile {name} needs at least --harness and --model");
            }
            // Rebuild the on-disk [profiles.<name>] shape: user entries only,
            // merged over the catalog (which already layers built-ins).
            let mut on_disk: BTreeMap<String, profile::Profile> = BTreeMap::new();
            for n in cat.names() {
                // carry local (non-builtin) entries verbatim; the edited one is
                // rewritten from the merge so partial edits keep built-in fields
                if (n == *name || !is_builtin_profile(&n)) && let Some(p) = cat.get(&n) {
                    on_disk.insert(n, p.clone());
                }
            }
            on_disk.insert(name.clone(), merged);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let raw = toml::to_string_pretty(&ProfilesFile { profiles: on_disk })?;
            std::fs::write(&path, raw)?;
            println!("wrote {} [profiles.{name}]", path.display());
        }
        ProfileCmd::Run { name, workflow, inputs } => {
            let cat = ProfileCatalog::load()?;
            let _inputs: Value = match inputs {
                Some(s) if !s.is_empty() => serde_json::from_str(s)
                    .map_err(|e| anyhow!("--inputs must be JSON: {e}"))?,
                _ => json!({}),
            };
            // Host-scope resolve: operator identity, no project lookup. The
            // operator npub is best-effort (empty string when unset).
            let operator_npub = std::env::var("TORON_OPERATOR_NPUB")
                .unwrap_or_else(|_| "operator".to_string());
            let resolved = cat.resolve(name, None, &operator_npub, &|_, _| None)?;
            let spec = &resolved.spec;

            // Spawn via the configured multiplexer backend (default herdr).
            let backend = backend::multiplexer_backend(cfg)?;
            let agent_name = format!("profile-{name}");
            // Find a pane in the default project workspace, or create one.
            let workspace = resolved_spec_workspace(cfg, backend.as_ref(), &spec.harness)?;
            let info = backend.agent_start(&agent_name, &spec.harness, &workspace, &[])?;
            println!(
                "profile {name} spawned: agent={} pane={} backend={}",
                info.name.as_deref().unwrap_or(&agent_name),
                info.pane_id,
                spec.backend
            );
            if let Some(wf) = workflow {
                println!(
                    "workflow {wf} invoke not yet wired (P3 edge) — spawned agent is ready for manual dispatch"
                );
            }
        }
    }
    Ok(())
}

/// `flywheel squad` — named groups of profiles (ADR-0005 P6). Legacy recipes
/// map onto squads via [`profile::Squad::from_recipe`]; `recipes list` stays
/// an alias (the `recipes`/`template` asset commands).
fn cmd_squad(cfg: &Config, cmd: &SquadCmd) -> Result<()> {
    use profile::{ProfileCatalog, Squad};

    match cmd {
        SquadCmd::List => {
            let cat = ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let mut rows: Vec<Value> = Vec::new();
            // Compiled-in squads are derived from the built-in profile set;
            // user-authored recipes.toml entries surface as squads (aliases).
            for (name, r) in &recipes {
                let squad = Squad::from_recipe(r);
                rows.push(json!({
                    "name": name,
                    "members": squad.counts,
                }));
            }
            // Built-in profile singles are listed as 1-member squads so a
            // fresh install still has squad surface with zero config.
            for name in cat.names() {
                if !recipes.contains_key(&name) {
                    rows.push(json!({
                        "name": name,
                        "members": { name: 1 },
                    }));
                }
            }
            if rows.is_empty() {
                println!("no squads defined (edit {})", config::config_dir().join("recipes.toml").display());
            } else {
                for row in &rows {
                    println!("{}", serde_json::to_string_pretty(row)?);
                }
            }
            Ok(())
        }
        SquadCmd::Show { name } => {
            let cat = ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let squad = match recipes.get(name) {
                Some(r) => Squad::from_recipe(r),
                None if cat.get(name).is_some() => {
                    let mut counts = std::collections::BTreeMap::new();
                    counts.insert(name.clone(), 1u32);
                    Squad { counts }
                }
                None => bail!("unknown squad '@{name}' (flywheel squad list)"),
            };
            let members: Vec<Value> = squad
                .members()
                .into_iter()
                .map(|(p, n)| json!({ "profile": p, "count": n }))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "name": name, "members": members }))?
            );
            Ok(())
        }
        SquadCmd::Spawn { name, project, json } => {
            let cat = ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let squad = match recipes.get(name) {
                Some(r) => Squad::from_recipe(r),
                None if cat.get(name).is_some() => {
                    let mut counts = std::collections::BTreeMap::new();
                    counts.insert(name.clone(), 1u32);
                    Squad { counts }
                }
                None => bail!("unknown squad '@{name}' (flywheel squad list)"),
            };
            if squad.counts.is_empty() {
                bail!("squad {name} has no members");
            }
            let operator_npub = std::env::var("TORON_OPERATOR_NPUB")
                .unwrap_or_else(|_| "operator".to_string());
            let backend = backend::multiplexer_backend(cfg)?;
            let mut spawned: Vec<Value> = Vec::new();
            for (profile_name, n) in squad.members() {
                let resolved = cat.resolve(profile_name, project.as_deref(), &operator_npub, &|_, _| None)?;
                let spec = &resolved.spec;
                for i in 0..n {
                    let agent_name = format!("squad-{name}-{profile_name}-{}", i + 1);
                    let workspace = resolved_spec_workspace(cfg, backend.as_ref(), &spec.harness)?;
                    let info = backend.agent_start(&agent_name, &spec.harness, &workspace, &[])?;
                    spawned.push(json!({
                        "squad": name,
                        "profile": profile_name,
                        "agent": info.name.as_deref().unwrap_or(&agent_name),
                        "pane": info.pane_id,
                        "backend": spec.backend,
                    }));
                    if !*json {
                        println!(
                            "spawned {profile_name}#{i} → agent={} pane={} backend={}",
                            info.name.as_deref().unwrap_or(&agent_name),
                            info.pane_id,
                            spec.backend
                        );
                    }
                }
            }
            if *json {
                println!("{}", serde_json::to_string_pretty(&json!({ "spawned": spawned }))?);
            }
            Ok(())
        }
    }
}

/// `flywheel runs` — the captain's daily dashboard (ADR-0005 P6.3). Reads the
/// per-project JSONL logs (workflow/pipeline/loop events) and aggregates them
/// into a run list with state + owner + advisory per-run cost.
fn cmd_runs(_cfg: &Config, cmd: &RunsCmd) -> Result<()> {
    use state::{EventKind, LogEntry};

    /// All projects with a log under the state dir.
    fn projects_with_logs() -> Vec<String> {
        let mut out = Vec::new();
        let dir = config::state_dir();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".jsonl") {
                    out.push(name.trim_end_matches(".jsonl").to_string());
                }
            }
        }
        out.sort();
        out
    }

    /// All run-kind log entries across projects, newest first.
    fn all_run_entries() -> Result<Vec<(String, LogEntry)>> {
        let mut out = Vec::new();
        for project in projects_with_logs() {
            for e in state::read_log(&project)? {
                match e.kind {
                    EventKind::Workflow | EventKind::Pipeline | EventKind::Loop => {
                        out.push((project.clone(), e));
                    }
                    _ => {}
                }
            }
        }
        out.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
        Ok(out)
    }

    /// Advisory per-run cost (best-effort; 0 when unrecorded).
    fn run_cost(e: &LogEntry) -> Value {
        let tokens = e.data.get("tokens").and_then(Value::as_u64).unwrap_or(0);
        let model = e.data.get("model").and_then(Value::as_str).unwrap_or("");
        json!({ "tokens": tokens, "model": model })
    }

    /// Owner of a run (workflow / pipeline / loop / agent named in payload).
    fn run_owner(e: &LogEntry) -> String {
        ["workflow", "pipeline", "loop", "agent"]
            .iter()
            .find_map(|k| e.data.get(*k).and_then(Value::as_str).map(String::from))
            .unwrap_or_default()
    }

    /// A run is pending when its payload carries a suspend/park marker.
    fn is_pending(e: &LogEntry) -> bool {
        e.data
            .get("status")
            .and_then(Value::as_str)
            .map(|s| matches!(s, "pending" | "awaiting_approval" | "parked"))
            .unwrap_or(false)
    }

    match cmd {
        RunsCmd::List => {
            let runs = all_run_entries()?;
            if runs.is_empty() {
                println!("no runs recorded");
                return Ok(());
            }
            let rows: Vec<Value> = runs
                .iter()
                .map(|(project, e)| {
                    json!({
                        "id": format!("{project}:{}", &e.ts),
                        "ts": e.ts,
                        "project": project,
                        "kind": format!("{:?}", e.kind).to_lowercase(),
                        "owner": run_owner(e),
                        "state": e.data.get("status").and_then(Value::as_str).unwrap_or("done"),
                        "cost": run_cost(e),
                    })
                })
                .collect();
            for r in &rows {
                println!("{}", serde_json::to_string_pretty(r)?);
            }
            Ok(())
        }
        RunsCmd::Show { id } => {
            let runs = all_run_entries()?;
            let hit = runs
                .iter()
                .find(|(project, e)| format!("{project}:{}", e.ts) == *id)
                .ok_or_else(|| anyhow!("run not found: {id} (flywheel runs list)"))?;
            print_json(&json!({
                "id": id,
                "project": hit.0,
                "kind": format!("{:?}", hit.1.kind).to_lowercase(),
                "ts": hit.1.ts,
                "owner": run_owner(&hit.1),
                "data": hit.1.data,
            }));
            Ok(())
        }
        RunsCmd::Pending => {
            let runs = all_run_entries()?;
            let pending: Vec<Value> = runs
                .iter()
                .filter(|(_, e)| is_pending(e))
                .map(|(project, e)| {
                    json!({
                        "id": format!("{project}:{}", e.ts),
                        "project": project,
                        "kind": format!("{:?}", e.kind).to_lowercase(),
                        "owner": run_owner(e),
                        "ts": e.ts,
                    })
                })
                .collect();
            if pending.is_empty() {
                println!("no pending runs");
            } else {
                for r in &pending {
                    println!("{}", serde_json::to_string_pretty(r)?);
                }
            }
            Ok(())
        }
        RunsCmd::Digest { since, broken_after } => {
            let runs = all_run_entries()?;
            let cutoff: Option<String> = match since {
                Some(s)
                    if s.ends_with('s')
                        || s.ends_with('m')
                        || s.ends_with('h')
                        || s.ends_with('d') =>
                {
                    let (num, unit) = s.split_at(s.len() - 1);
                    let n: u64 = num.parse().map_err(|_| anyhow!("bad --since {s}"))?;
                    let secs = match unit {
                        "s" => n,
                        "m" => n * 60,
                        "h" => n * 3600,
                        "d" => n * 86400,
                        _ => unreachable!(),
                    };
                    Some(now_minus(secs))
                }
                Some(s) => Some(s.clone()),
                None => None,
            };
            let recent: Vec<(String, LogEntry)> = match &cutoff {
                Some(c) => runs
                    .iter()
                    .filter(|(_, e)| e.ts >= *c)
                    .cloned()
                    .collect(),
                None => runs.clone(),
            };
            let failed: Vec<Value> = recent
                .iter()
                .filter(|(_, e)| {
                    e.data
                        .get("status")
                        .and_then(Value::as_str)
                        .map(|s| matches!(s, "failed" | "error"))
                        .unwrap_or(false)
                })
                .map(|(project, e)| {
                    json!({
                        "id": format!("{project}:{}", e.ts),
                        "project": project,
                        "owner": run_owner(e),
                        "ts": e.ts,
                    })
                })
                .collect();
            let parked = recent.iter().filter(|(_, e)| is_pending(e)).count();
            let mut per_owner: std::collections::HashMap<String, u32> =
                std::collections::HashMap::new();
            for (_, e) in &recent {
                let owner = run_owner(e);
                let is_fail = e
                    .data
                    .get("status")
                    .and_then(Value::as_str)
                    .map(|s| matches!(s, "failed" | "error"))
                    .unwrap_or(false);
                *per_owner.entry(owner).or_insert(0) += u32::from(is_fail);
            }
            let broken: Vec<Value> = per_owner
                .iter()
                .filter(|(_, n)| **n > *broken_after)
                .map(|(owner, n)| json!({ "owner": owner, "failures": n }))
                .collect();
            print_json(&json!({
                "since": cutoff,
                "runs": recent.len(),
                "failed": failed,
                "parked": parked,
                "broken_gt_n": broken,
            }));
            Ok(())
        }
    }
}

/// `flywheel workflow` — the unified orchestration surface (ADR-0005 Spec 5 /
/// P3.7). Wires the dead-code modules (`crew`, `workflow_catalog`,
/// `builtin_workflows`) into the CLI: catalog listing, the fail-stop lint
/// gate, engine-backed `run`, scaffolding, history, and the auto-crew
/// preview.
fn cmd_workflow(cmd: &WorkflowCmd) -> Result<()> {
    use toron_workflow::schema::{Action, CrewSpec, Workflow};
    use toron_workflow::{lint_workflow, Level};

    /// The full workflow set: built-ins (compiled-in) + catalog (canonical
    /// YAML + absorbed legacy), name → (workflow, source).
    fn all_workflows() -> Result<BTreeMap<String, (Workflow, &'static str)>> {
        let mut out: BTreeMap<String, (Workflow, &'static str)> = BTreeMap::new();
        for (name, wf) in builtin_workflows::builtin_workflows() {
            out.insert(name, (wf, "builtin"));
        }
        let cat = workflow_catalog::WorkflowCatalog::load()?;
        for (name, wf) in cat.workflows {
            // Canonical YAML / absorbed legacy wins over a same-named built-in
            // (the catalog is the home; built-ins are zero-config defaults).
            out.insert(name, (wf, "catalog"));
        }
        Ok(out)
    }

    /// Lint one workflow; print issues; return the blocking (Error-level) count.
    fn lint_one(name: &str, wf: &Workflow) -> usize {
        // Engine tier (toron-workflow lint): schema, guards (evalexpr +
        // timeout), template refs, graph acyclicity — inherited, fail-stop.
        let report = lint_workflow(wf);
        let mut blocking = report
            .issues
            .iter()
            .filter(|i| i.level == Level::Error)
            .count();
        for issue in &report.issues {
            eprintln!(
                "[{name}] {:>7} {}",
                if issue.level == Level::Error {
                    "error"
                } else {
                    "warning"
                },
                issue.message
            );
        }
        // Flywheel tier (ADR-0005 lint-gate contract — the ENGINE cannot do
        // these: no dependency inversion means it never imports flywheel's
        // StageLibrary / ProfileCatalog). Owned here, fail-stop.
        blocking += lint_flywheel_tier(name, wf);
        blocking
    }

    /// Resolve a single workflow by name across built-ins + catalog.
    fn resolve_workflow<'a>(
        name: &str,
        all: &'a BTreeMap<String, (Workflow, &'static str)>,
    ) -> Result<&'a Workflow> {
        all.get(name)
            .map(|(wf, _)| wf)
            .ok_or_else(|| anyhow!("unknown workflow {name:?} (flywheel workflow list)"))
    }

    match cmd {
        WorkflowCmd::List => {
            let all = all_workflows()?;
            if all.is_empty() {
                println!("no workflows defined");
                return Ok(());
            }
            let mut rows: Vec<Value> = Vec::new();
            for (name, (wf, source)) in &all {
                rows.push(json!({
                    "name": name,
                    "source": source,
                    "trigger": wf.trigger.as_ref().map(|t| t.type_name()),
                    "crew": match &wf.crew {
                        Some(CrewSpec::Auto) => "auto",
                        Some(CrewSpec::Explicit(_)) => "explicit",
                        None => "none",
                    },
                    "steps": wf.steps.len(),
                    "graph_nodes": wf.graph.as_ref().map(|g| g.nodes.len()),
                    "inputs": wf.inputs.iter().map(|i| &i.name).collect::<Vec<_>>(),
                }));
            }
            print_json(&json!({ "workflows": rows }));
            Ok(())
        }
        WorkflowCmd::Show { name } => {
            let all = all_workflows()?;
            let (wf, source) = all
                .get(name)
                .ok_or_else(|| anyhow!("unknown workflow {name:?} (flywheel workflow list)"))?;
            let crew = match &wf.crew {
                Some(CrewSpec::Auto) => "auto".to_string(),
                Some(CrewSpec::Explicit(m)) => serde_json::to_string(m)?,
                None => "none".to_string(),
            };
            let step_labels: Vec<String> = wf
                .steps
                .iter()
                .map(|s| {
                    let n = s.name.clone().unwrap_or_else(|| "<unnamed>".to_string());
                    format!("{n}: {}", s.action.type_name())
                })
                .collect();
            print_json(&json!({
                "name": wf.name,
                "source": source,
                "description": wf.description,
                "trigger": wf.trigger.as_ref().map(|t| t.type_name()),
                "guard": wf.guard,
                "crew": crew,
                "inputs": wf.inputs.iter().map(|i| &i.name).collect::<Vec<_>>(),
                "steps": step_labels,
                "graph_nodes": wf.graph.as_ref().map(|g| g.nodes.len()),
            }));
            Ok(())
        }
        WorkflowCmd::Lint { name } => {
            let all = all_workflows()?;
            match name {
                Some(n) => {
                    let (wf, _) = all.get(n).ok_or_else(|| {
                        anyhow!("unknown workflow {n:?} (flywheel workflow list)")
                    })?;
                    let blocking = lint_one(n, wf);
                    if blocking > 0 {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                None => {
                    let mut blocking_total = 0;
                    for (n, (wf, _)) in &all {
                        blocking_total += lint_one(n, wf);
                    }
                    if blocking_total > 0 {
                        std::process::exit(1);
                    }
                    println!("lint clean: {} workflow(s)", all.len());
                    Ok(())
                }
            }
        }
        WorkflowCmd::Run {
            name,
            inputs,
            crew,
            json,
        } => {
            let all = all_workflows()?;
            let wf = resolve_workflow(name, &all)?.clone();
            // Fail-stop gate: lint before running.
            let blocking = lint_one(name, &wf);
            if blocking > 0 {
                bail!("workflow {name:?} has {blocking} blocking lint issue(s) — refusing to run");
            }
            let inputs_value: Value = match inputs {
                Some(s) if !s.trim().is_empty() => {
                    serde_json::from_str(s).map_err(|e| anyhow!("--inputs must be JSON: {e}"))?
                }
                _ => json!({}),
            };
            // Crew resolution: auto → infer from the workflow; <squad> →
            // explicit role:count. Used for the preview + the engine invoke.
            let crew_plan = match crew.as_deref() {
                None | Some("auto") => crew::plan_crew(&wf),
                Some(squad) => {
                    let cat = profile::ProfileCatalog::load()?;
                    let recipes = config::Recipes::load()?.recipes;
                    let squad_counts = match recipes.get(squad) {
                        Some(r) => profile::Squad::from_recipe(r).counts,
                        None if cat.get(squad).is_some() => {
                            let mut m = BTreeMap::new();
                            m.insert(squad.to_string(), 1u32);
                            m
                        }
                        None => {
                            bail!("unknown squad {squad:?} (flywheel squad list)")
                        }
                    };
                    crew::CrewPlan {
                        members: squad_counts,
                    }
                }
            };
            // A recording action runner: flywheel's CLI run exercises the
            // engine (delays, approvals park durably, graph scheduling,
            // control-flow); action side-effects are recorded, not dispatched
            // (herdr dispatch is the P4 spawn-await binding).
            let runner: toron_workflow::ActionRunner =
                Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
            let rt = tokio::runtime::Runtime::new()?;
            let run_id = format!("fw-cli-{}", state::now_iso());
            let output = rt.block_on(async {
                let engine = toron_workflow::WorkflowEngine::local(runner)?;
                engine
                    .invoke(&run_id, wf.clone(), inputs_value.clone())
                    .await
            })?;
            log(
                "workflow",
                EventKind::Workflow,
                json!({
                    "workflow": name,
                    "status": "done",
                    "crew": crew_plan.members,
                    "run_id": run_id,
                }),
            );
            if *json {
                print_json(&json!({
                    "workflow": name,
                    "run_id": run_id,
                    "crew": crew_plan.members,
                    "output": output,
                }));
            } else {
                println!(
                    "workflow {name} ran (run_id {run_id}, crew {} member(s))",
                    crew_plan.members.len()
                );
                print_json(&output);
            }
            Ok(())
        }
        WorkflowCmd::New { name } => {
            if name.trim().is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
            {
                bail!("invalid workflow name {name:?} (alnum/_/-/.)");
            }
            let dir = config::config_dir().join("workflows");
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            let path = dir.join(format!("{name}.yaml"));
            if path.exists() {
                bail!("workflow already exists: {}", path.display());
            }
            let scaffold = format!(
                "name: {name}\ndescription: \"\"\ntrigger:\n  type: webhook\ninputs:\n  - name: goal\nsteps:\n  - name: step-1\n    action: send_dm\n    to: operator\n    body: \"started {{name}}\"\n"
            );
            std::fs::write(&path, scaffold)?;
            println!("wrote {}", path.display());
            Ok(())
        }
        WorkflowCmd::History { name } => {
            use state::EventKind;
            let dir = config::state_dir();
            let mut rows: Vec<Value> = Vec::new();
            let Ok(entries) = std::fs::read_dir(&dir) else {
                print_json(&json!({ "workflow": name, "runs": [] }));
                return Ok(());
            };
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if !fname.ends_with(".jsonl") {
                    continue;
                }
                let project = fname.trim_end_matches(".jsonl").to_string();
                let Ok(events) = state::read_log(&project) else {
                    continue;
                };
                for e in events {
                    if e.kind != EventKind::Workflow {
                        continue;
                    }
                    let owner = e
                        .data
                        .get("workflow")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if owner == *name {
                        rows.push(json!({
                            "ts": e.ts,
                            "project": project,
                            "run_id": e.data.get("run_id"),
                            "status": e.data.get("status").and_then(Value::as_str).unwrap_or("done"),
                            "crew": e.data.get("crew"),
                        }));
                    }
                }
            }
            rows.sort_by(|a, b| {
                b.get("ts")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(a.get("ts").and_then(Value::as_str).unwrap_or(""))
            });
            print_json(&json!({ "workflow": name, "runs": rows }));
            Ok(())
        }
        WorkflowCmd::Crew { name } => {
            let all = all_workflows()?;
            let wf = resolve_workflow(name, &all)?;
            let plan = crew::plan_crew(wf);
            let members: Vec<Value> = plan
                .members
                .iter()
                .map(|(role, count)| json!({ "role": role, "count": count }))
                .collect();
            if members.is_empty() {
                println!("workflow {name} has no crew (crew omitted or no profile refs)");
                return Ok(());
            }
            print_json(&json!({ "workflow": name, "crew": members }));
            Ok(())
        }
    }
}

/// `flywheel cron` — the smart-cron catalog + control surface (ADR-0005
/// Spec 3 / P4.6). The merged view spans the `crons.toml` registry (written
/// by `add`/`pause`/`resume`) and every schedule-triggered workflow in the
/// catalog + built-ins. Status is derived from the state log; the P4.3
/// durable-sleep manager will drive ticks off the same registry.
fn cmd_cron(cmd: &CronCmd) -> Result<()> {
    use cron::{CronManager, CronStatus, MergedCron, RegistryCronManager, row_json};
    use toron_workflow::schema::CronScope;

    /// The manager the CLI drives. Today: the registry-backed manager
    /// (crons.toml + catalog + state log). When P4.3 lands, swap in the
    /// durable-sleep cron manager — the CLI compiles against [`CronManager`],
    /// never against a concrete impl.
    fn make_manager() -> Result<Box<dyn CronManager>> {
        Ok(Box::new(RegistryCronManager::load()?))
    }

    fn render(mc: &MergedCron, st: &CronStatus, json: bool) -> Result<()> {
        let row = row_json(mc, st.status, st.next_due);
        if json {
            println!("{}", serde_json::to_string_pretty(&row)?);
        } else {
            println!(
                "{:<28} {:<8} {:<8} {:<22} {:<6} {:<9} {:<8}",
                mc.entry.name,
                mc.entry.template,
                mc.entry.cron,
                mc.entry.owner,
                match mc.entry.scope {
                    CronScope::Host => "host",
                    CronScope::Project => "project",
                },
                st.status,
                if mc.entry.enabled { "on" } else { "paused" },
            );
        }
        Ok(())
    }

    let manager = make_manager()?;

    match cmd {
        CronCmd::List { json } => {
            let merged = manager.list()?;
            if merged.is_empty() {
                println!("no crons defined");
                return Ok(());
            }
            let statuses = manager.status_all()?;
            if *json {
                let rows: Vec<Value> = merged
                    .iter()
                    .map(|mc| {
                        let st = statuses
                            .iter()
                            .find(|s| s.name == mc.entry.name)
                            .expect("status for every listed cron");
                        row_json(mc, st.status, st.next_due)
                    })
                    .collect();
                print_json(&json!({ "crons": rows }));
            } else {
                println!(
                    "{:<28} {:<8} {:<8} {:<22} {:<6} {:<9} {:<8}",
                    "NAME", "TEMPLATE", "CRON", "OWNER", "SCOPE", "STATUS", "STATE"
                );
                for mc in &merged {
                    let st = statuses
                        .iter()
                        .find(|s| s.name == mc.entry.name)
                        .expect("status for every listed cron");
                    render(mc, st, false)?;
                }
            }
            Ok(())
        }
        CronCmd::Show { name, json } => {
            let mc = manager
                .show(name)?
                .ok_or_else(|| anyhow!("unknown cron {name:?} (flywheel cron list)"))?;
            let st = manager
                .status(name)?
                .ok_or_else(|| anyhow!("unknown cron {name:?} (flywheel cron list)"))?;
            if *json {
                render(&mc, &st, true)?;
            } else {
                println!("name:     {}", mc.entry.name);
                println!("template: {}", mc.entry.template);
                println!("cron:     {}", mc.entry.cron);
                println!("owner:    {}", mc.entry.owner);
                println!(
                    "scope:    {}",
                    match mc.entry.scope {
                        CronScope::Host => "host",
                        CronScope::Project => "project",
                    }
                );
                println!("source:   {}", mc.source);
                println!("status:   {}", st.status);
                if let Some(d) = st.next_due {
                    println!("next_due: {d}");
                }
                if let Some(wf) = manager_workflow(name) && let Some(p) = &wf.policy {
                    println!("policy:   {p:?}");
                }
            }
            Ok(())
        }
        CronCmd::Add {
            name,
            template,
            cron,
            owner,
            scope,
            json,
        } => {
            let scope = match scope.as_deref() {
                Some("host") | None => CronScope::Host,
                Some("project") => CronScope::Project,
                Some(other) => bail!("scope must be host or project, got {other:?}"),
            };
            let mut manager = manager;
            manager.add(cron::CronEntry {
                name: name.clone(),
                template: template.clone(),
                cron: cron.clone(),
                owner: owner.clone(),
                scope,
                enabled: true,
                created_at: chrono::Utc::now().to_rfc3339(),
            })?;
            if *json {
                let entry = manager
                    .show(name)?
                    .expect("added cron is present");
                let st = manager.status(name)?.expect("added cron has status");
                render(&entry, &st, true)?;
            } else {
                println!("added cron {name} (template {template}, {cron}, owner {owner})");
            }
            Ok(())
        }
        CronCmd::Pause { name } => {
            let mut manager = manager;
            manager.set_enabled(name, false)?;
            println!("paused cron {name}");
            Ok(())
        }
        CronCmd::Resume { name } => {
            let mut manager = manager;
            manager.set_enabled(name, true)?;
            println!("resumed cron {name}");
            Ok(())
        }
        CronCmd::Status { json } => {
            let statuses = manager.status_all()?;
            if statuses.is_empty() {
                println!("no crons defined");
                return Ok(());
            }
            let rows: Vec<Value> = statuses
                .iter()
                .map(|st| {
                    json!({
                        "name": st.name,
                        "status": st.status,
                        "enabled": st.enabled,
                        "next_due": st.next_due,
                    })
                })
                .collect();
            if *json {
                print_json(&json!({ "crons": rows }));
            } else {
                println!("{:<28} {:<9} {:<8} {:<12}", "NAME", "STATUS", "STATE", "NEXT_DUE");
                for row in &rows {
                    println!(
                        "{:<28} {:<9} {:<8} {}",
                        row["name"].as_str().unwrap_or(""),
                        row["status"].as_str().unwrap_or(""),
                        if row["enabled"].as_bool().unwrap_or(false) { "on" } else { "paused" },
                        row["next_due"].as_i64().unwrap_or(0),
                    );
                }
            }
            Ok(())
        }
        CronCmd::History { name, json } => {
            let runs = manager.history(name)?;
            let rows: Vec<Value> = runs
                .iter()
                .map(|r| {
                    json!({
                        "ts": r.ts,
                        "project": r.project,
                        "run_id": r.run_id,
                        "status": r.status,
                        "bead": r.bead,
                    })
                })
                .collect();
            if *json {
                print_json(&json!({ "cron": name, "runs": rows }));
            } else if rows.is_empty() {
                println!("no runs recorded for cron {name}");
            } else {
                for row in &rows {
                    println!(
                        "{} {} {} {}",
                        row["ts"].as_str().unwrap_or(""),
                        row["run_id"].as_str().unwrap_or(""),
                        row["status"].as_str().unwrap_or(""),
                        row["bead"].as_str().unwrap_or(""),
                    );
                }
            }
            Ok(())
        }
        CronCmd::Start => {
            // Blocks until Ctrl-C: durable loops + policy-sink drain (bead
            // side-effects). Run under the project dir so `br create` lands
            // in the right .beads.
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cron::start_loops())
        }
    }
}

/// Look up a workflow by name across the catalog for the `show` policy line.
fn manager_workflow(name: &str) -> Option<toron_workflow::schema::Workflow> {
    let cat = workflow_catalog::WorkflowCatalog::load().ok()?;
    cat.workflows.get(name).cloned().or_else(|| {
        builtin_workflows::builtin_workflows()
            .get(name)
            .cloned()
    })
}

/// The flywheel-tier lint checks (the bead's fail-stop contract): profile
/// `@name` resolution against the ProfileCatalog, `stage:` alias resolution
/// against the StageLibrary, and `{{inputs.x}}` presence against the declared
/// `inputs:`. The ENGINE cannot do these (no dependency inversion — it never
/// imports flywheel's StageLibrary / ProfileCatalog), so the CLI owns them
/// and fails closed before any run.
fn lint_flywheel_tier(name: &str, wf: &toron_workflow::schema::Workflow) -> usize {
    let profiles = profile::ProfileCatalog::load().unwrap_or_default();
    let stages = stages::StageLibrary::load().unwrap_or_default();
    let declared_inputs: std::collections::HashSet<&str> =
        wf.inputs.iter().map(|i| i.name.as_str()).collect();
    let mut blocking = 0;

    // Emit an error line for this workflow (name captured).
    let emit = |msg: &str, blocking: &mut usize| {
        eprintln!("[{name}]    error {msg}");
        *blocking += 1;
    };

    fn check_profile(
        profile: &str,
        profiles: &profile::ProfileCatalog,
        blocking: &mut usize,
        emit: &dyn Fn(&str, &mut usize),
    ) {
        let bare = profile.trim_start_matches('@');
        if bare.is_empty() {
            return;
        }
        if !profiles.get(bare).is_some() {
            let msg = format!(
                "profile {profile:?} does not resolve (known: {})",
                profiles.names().join(", ")
            );
            emit(&msg, blocking);
        }
    }

    fn collect_input_refs(
        v: &serde_json::Value,
        declared: &std::collections::HashSet<&str>,
        blocking: &mut usize,
        emit: &dyn Fn(&str, &mut usize),
    ) {
        match v {
            serde_json::Value::String(s) => {
                for inner in template_tokens(s) {
                    if let Some(key) = inner.strip_prefix("inputs.") {
                        // Nested refs (`{{inputs.bundle.run_id}}`) resolve
                        // against the declared ROOT input (`bundle`); the
                        // engine drills into the object at runtime.
                        let root = key.split('.').next().unwrap_or(key);
                        if !declared.contains(root) {
                            let msg = format!(
                                "{{{{inputs.{key}}}}} references input {root:?} that is not declared in `inputs:`"
                            );
                            emit(&msg, blocking);
                        }
                    }
                }
            }
            serde_json::Value::Array(a) => {
                for item in a {
                    collect_input_refs(item, declared, blocking, emit);
                }
            }
            serde_json::Value::Object(o) => {
                for v in o.values() {
                    collect_input_refs(v, declared, blocking, emit);
                }
            }
            _ => {}
        }
    }

    fn template_tokens(s: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(start) = rest.find("{{") {
            if let Some(end) = rest[start + 2..].find("}}") {
                out.push(&rest[start + 2..start + 2 + end]);
                rest = &rest[start + 2 + end + 2..];
            } else {
                break;
            }
        }
        out
    }

    // Walk one action: dispatch_job spec (profile + stage), spawn profile,
    // and {{inputs.X}} refs across all string leaves.
    fn scan_action(
        action: &toron_workflow::schema::Action,
        profiles: &profile::ProfileCatalog,
        stages: &stages::StageLibrary,
        declared: &std::collections::HashSet<&str>,
        blocking: &mut usize,
        emit: &dyn Fn(&str, &mut usize),
    ) {
        let json = serde_json::to_value(action).unwrap_or_default();
        if let Some(spec) = json.get("spec").and_then(|s| s.as_object()) {
            if let Some(profile) = spec.get("profile").and_then(|v| v.as_str()) {
                check_profile(profile, profiles, blocking, emit);
            }
            if let Some(stage) = spec.get("stage").and_then(|v| v.as_str()) && !stages.has(stage) {
                emit(
                    &format!("stage {stage:?} does not resolve (semantic or p-code alias)"),
                    blocking,
                );
            }
        }
        if let Some(profile) = json.get("profile").and_then(|v| v.as_str()) {
            check_profile(profile, profiles, blocking, emit);
        }
        for v in json.as_object().into_iter().flat_map(|o| o.values()) {
            collect_input_refs(v, declared, blocking, emit);
        }
    }

    for step in &wf.steps {
        scan_action(
            &step.action,
            &profiles,
            &stages,
            &declared_inputs,
            &mut blocking,
            &emit,
        );
    }
    if let Some(graph) = &wf.graph {
        for node in graph.nodes.values() {
            scan_action(
                &node.action,
                &profiles,
                &stages,
                &declared_inputs,
                &mut blocking,
                &emit,
            );
        }
    }
    blocking
}

/// ISO timestamp `now - secs` (for `runs digest --since`).
fn now_minus(secs: u64) -> String {
    let (now, _) = state::utc_now();
    let (y, mo, d, h, mi, s) = state::civil_from_unix(now.saturating_sub(secs));
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// True when a profile is one of the 6 compiled-in built-ins (so `create`/
/// `edit` doesn't duplicate it on disk verbatim).
fn is_builtin_profile(name: &str) -> bool {
    matches!(
        name,
        "oracle" | "planner" | "reviewer" | "coder" | "scout" | "housekeeper"
    )
}

/// Resolve a pane id in the default project workspace for `profile run`:
/// reuse the first workspace whose label matches a default project, else the
/// first workspace, else create one.
fn resolved_spec_workspace(
    _cfg: &Config,
    backend: &dyn backend::MultiplexerBackend,
    _harness: &str,
) -> Result<String> {
    let ws = backend.workspace_list()?;
    if let Some(first) = ws.first() {
        return Ok(first.workspace_id.clone());
    }
    let created = backend.workspace_create(None)?;
    Ok(created.workspace_id)
}

/// On-disk shape of `profiles.toml` for the CLI writer.
#[derive(serde::Serialize)]
struct ProfilesFile {
    profiles: BTreeMap<String, profile::Profile>,
}

/// `flywheel debate <proposition> [--rounds N] [--agent-a ..] [--agent-b ..]
/// [--judge ..]` — N-round A/B debate + judge (ADR-0001 §9.1).
///
/// Threaded (thread_id = debate-<ts>), ack-gated (turn advances on the
/// agent settling — done/idle — then the orchestrator harvests the speaker's
/// latest message via search_messages). Judge reads via summarize_thread.
///
/// governed-by: ADR-0001 (bd-pattern-debate-loop)
fn cmd_debate(
    cfg: &Config,
    proposition: &str,
    rounds: u32,
    agent_a: Option<&str>,
    agent_b: Option<&str>,
    judge: Option<&str>,
) -> Result<()> {
    let rounds = rounds.max(1);
    // Compact ts for the thread id: ISO-8601 (`2026-08-06T13:03:42Z`) is NOT a
    // valid thread id — toron's send_message validator (AM-Rust parity) requires
    // `[a-zA-Z0-9._-]` only, no `:`. Replace hyphens AND colons AND the trailing
    // Z so the id is e.g. `debate-20260806T130548`.
    let iso = crate::state::now_iso().replacen('-', "", 10);
    let ts: String = iso.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let thread = format!("debate-{ts}");

    // scope the debate to the project dir (the proposition may be a project
    // name; default to the flywheel project for ad-hoc debates)
    let project = if cfg.workspace_root().join(proposition).exists() {
        proposition.to_string()
    } else {
        "flywheel".to_string()
    };
                let key = project_dir_str(cfg, &project)?;

    // agent names: explicit or default to the swarm's first oc/pi/gk
    // (review-fix F7: bare shorts resolve to the spawned flywheel-<base>-<short>
    // so settle + mail addressing work against a default swarm)
    let (a, b, j) = resolve_debate_agents(&project, agent_a, agent_b, judge)?;

    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => bail!("mail backend unavailable for debate: {e:#}"),
    };
    let mux = backend::multiplexer_backend(cfg)?;
    backend::run_mail(mail.ensure_project(&key))?;

    let mail_target = |agent: &str| resolve_mail_target(agent).to_string();

    // opening: A argues FOR
    let opening = format!(
        "You are debating proposition: {proposition}\n\nArgue FOR it. Be concise and concise (<= 120 words). Provide your single strongest argument with a brief reason.\n\nAfter replying in this thread, acknowledge the orchestrator's opening message to signal your turn is done."
    );
    mail_send(
        &*mail,
        &key,
        &[mail_target(&a)],
        &format!("[debate] {proposition}"),
        &opening,
        "high",
        true,
        Some(&thread),
    )?;
    println!("debate {thread}: {} opens FOR (round 1)", a);

    let mut last_reply: Option<String> = None;
    for round in 1..=rounds {
        // A speaks on odd rounds (round 1 opening + counters), B on even
        let speaker: &str = if round % 2 == 1 { &a } else { &b };
        if round > 1 {
            let reply = last_reply.clone().unwrap_or_default();
            let counter = format!(
                "You are debating proposition: {proposition}\n\nCounter the opponent's latest argument with a NEW point (do not repeat). <= 120 words. Acknowledge when done.\n\nOpponent's latest argument: {reply}"
            );
            mail_send(
                &*mail,
                &key,
                &[mail_target(speaker)],
                &format!("[debate] {proposition}"),
                &counter,
                "high",
                true,
                Some(&thread),
            )?;
        }
        // settle: wait for the speaker to reach done/idle (the ack-gate proxy;
        // full ack-checking is the loop's ack_overdue path)
        settle_agent(&*mux, speaker)?;
        // harvest the speaker's latest message in the thread
        last_reply = harvest_latest(mail.as_ref(), &key, &thread, &resolve_mail_target(speaker))?;
        println!("debate {thread}: {} replied (round {})", speaker, round);
    }

    // judge: read the thread, pick a winner
    let judge_prompt = format!(
        "You are the judge of this debate on: {proposition}\n\nRead the full thread via summarize_thread. Pick a winner in <= 100 words: name the winner's argument and why it is stronger. Reply in thread and acknowledge when done."
    );
    mail_send(
        &*mail,
        &key,
        &[mail_target(&j)],
        &format!("[debate] {proposition} — judge"),
        &judge_prompt,
        "high",
        true,
        Some(&thread),
    )?;
    settle_agent(&*mux, &j)?;
    let verdict = harvest_latest(mail.as_ref(), &key, &thread, &resolve_mail_target(&j))?;
    println!(
        "debate {thread} verdict: {}",
        verdict.unwrap_or_else(|| "(no verdict harvested)".into())
    );
    Ok(())
}

// ---- plan-review-implement pattern (bd-pattern-plan-review-implement) ----
//
// Paper-first gate: implementer works on a branch and does not merge; the
// reviewer's VERIFIED does not close beads — a human/overseer signs off.
// Structured tokens only (APPROVE|CHANGES:|VERIFIED|REJECT:) — no free-text
// routing. Build-slot 'cargo' must be acquired before cargo (agents self-gate).

/// Reviewer structured tokens (leading token required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewToken {
    Approve,
    Changes(String),
    Verified,
    Reject(String),
}

/// Parse the first structured token from a reviewer reply.
pub fn parse_review_token(text: &str) -> Option<ReviewToken> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // first non-empty line carries the token
    let line = t
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(t);
    let upper = line.to_ascii_uppercase();
    if upper.starts_with("APPROVE") {
        return Some(ReviewToken::Approve);
    }
    if upper.starts_with("VERIFIED") {
        return Some(ReviewToken::Verified);
    }
    if upper.starts_with("CHANGES") {
        let notes = line
            .split_once(':')
            .map(|(_, n)| n.trim().to_string())
            .unwrap_or_default();
        return Some(ReviewToken::Changes(notes));
    }
    if upper.starts_with("REJECT") {
        let notes = line
            .split_once(':')
            .map(|(_, n)| n.trim().to_string())
            .unwrap_or_default();
        return Some(ReviewToken::Reject(notes));
    }
    None
}

/// Gate phase for the plan→review→implement→verify state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriPhase {
    AwaitingPlan,
    AwaitingPlanReview,
    AwaitingImplement,
    AwaitingVerify,
    Done,
    Escalated,
}

/// Next orchestrator action after a phase transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriAction {
    PromptPlanner { msg: String },
    PromptReviewer { msg: String },
    PromptImplementer { msg: String },
    HaltVerified,
    Escalate { reason: String },
}

/// Initial planner prompt (writes plan file + replies with path).
pub fn pri_planner_prompt(feature: &str, plan_path: &str) -> String {
    format!(
        "You are the PLANNER for this feature (paper-first gate).\n\n\
         Feature:\n{feature}\n\n\
         Write a concrete implementation plan to `{plan_path}`.\n\
         When done, reply in this thread with a single line:\n\
         PLAN: {plan_path}\n\
         Do not implement code. Do not merge."
    )
}

/// Route a plan-review token at the current phase.
///
/// Returns (next_phase, action, updated_change_rounds).
/// `change_rounds` counts completed CHANGES loops; escalate when the next
/// CHANGES would exceed `max_change_rounds`.
pub fn pri_route(
    phase: PriPhase,
    token: &ReviewToken,
    plan_path: &str,
    change_rounds: u32,
    max_change_rounds: u32,
) -> (PriPhase, PriAction, u32) {
    match (phase, token) {
        (PriPhase::AwaitingPlanReview, ReviewToken::Approve) => (
            PriPhase::AwaitingImplement,
            PriAction::PromptImplementer {
                msg: format!(
                    "You are the IMPLEMENTER (paper-first).\n\n\
                     Implement the plan at `{plan_path}`.\n\
                     - Reserve files via agent-mail (reason from the plan/bead).\n\
                     - Before `cargo build`/`cargo test`, acquire build-slot `cargo` \
                       (ttl ~1800s); release on completion.\n\
                     - Work on a branch — do NOT merge to main.\n\
                     - Do NOT close beads; the verifier + human own that.\n\
                     When done, reply IMPLEMENT_DONE in this thread."
                ),
            },
            change_rounds,
        ),
        (PriPhase::AwaitingPlanReview, ReviewToken::Changes(notes)) => {
            let next = change_rounds.saturating_add(1);
            if next > max_change_rounds {
                (
                    PriPhase::Escalated,
                    PriAction::Escalate {
                        reason: format!(
                            "change rounds exhausted (max {max_change_rounds}): {notes}"
                        ),
                    },
                    next,
                )
            } else {
                (
                    PriPhase::AwaitingPlan,
                    PriAction::PromptPlanner {
                        msg: format!(
                            "Changes requested (round {next}/{max_change_rounds}):\n{notes}\n\n\
                             Revise `{plan_path}`. Reply PLAN: {plan_path} when done."
                        ),
                    },
                    next,
                )
            }
        }
        (PriPhase::AwaitingPlanReview, ReviewToken::Reject(reason)) => (
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("plan rejected: {reason}"),
            },
            change_rounds,
        ),
        (PriPhase::AwaitingVerify, ReviewToken::Verified) => {
            (PriPhase::Done, PriAction::HaltVerified, change_rounds)
        }
        (PriPhase::AwaitingVerify, ReviewToken::Reject(reason)) => (
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("implementation rejected: {reason}"),
            },
            change_rounds,
        ),
        (PriPhase::AwaitingVerify, ReviewToken::Changes(notes)) => (
            // treat CHANGES at verify as reject-with-notes (no more impl loop here)
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("verify requested changes (escalate): {notes}"),
            },
            change_rounds,
        ),
        (phase, other) => (
            PriPhase::Escalated,
            PriAction::Escalate {
                reason: format!("unexpected token {other:?} in phase {phase:?}"),
            },
            change_rounds,
        ),
    }
}

/// After implementer settles: send verify prompt to reviewer.
pub fn pri_after_implement(plan_path: &str) -> (PriPhase, PriAction) {
    (
        PriPhase::AwaitingVerify,
        PriAction::PromptReviewer {
            msg: format!(
                "You are the REVIEWER (verify step, read-only).\n\n\
                 Verify the implementation matches the plan at `{plan_path}`.\n\
                 Do NOT edit files. Check for mid-edit conflicts before reading.\n\
                 Reply with first token VERIFIED or REJECT:<reason>."
            ),
        },
    )
}

/// After planner settles with a plan path: send review prompt.
pub fn pri_after_plan(plan_path: &str) -> (PriPhase, PriAction) {
    (
        PriPhase::AwaitingPlanReview,
        PriAction::PromptReviewer {
            msg: format!(
                "You are the REVIEWER (plan gate).\n\n\
                 Review the plan at `{plan_path}`.\n\
                 Reply with first token APPROVE or CHANGES:<notes>.\n\
                 Do not implement. Do not merge."
            ),
        },
    )
}

/// Extract `PLAN: <path>` from planner reply (best-effort).
pub fn extract_plan_path(text: &str, fallback: &str) -> String {
    for line in text.lines() {
        let t = line.trim();
        let upper = t.to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("PLAN:") {
            // use original casing from the line after the colon
            if let Some((_, path)) = t.split_once(':') {
                let p = path.trim();
                if !p.is_empty() {
                    return p.to_string();
                }
            }
            let p = rest.trim();
            if !p.is_empty() {
                return p.to_string();
            }
        }
    }
    fallback.to_string()
}

/// `flywheel plan-review <project> <feature> [--max-change-rounds 2] ...`
///
/// Threaded gate (thread_id = plan-<bead|ts>). Paper-first: implementer does
/// not merge; VERIFIED does not close beads.
///
/// governed-by: ADR-0001 §9.2 (bd-pattern-plan-review-implement)
/// clap command signature — params map 1:1 to CLI flags.
#[allow(clippy::too_many_arguments)]
fn cmd_plan_review(
    cfg: &Config,
    project: &str,
    feature: &str,
    max_change_rounds: u32,
    planner: Option<&str>,
    reviewer: Option<&str>,
    implementer: Option<&str>,
    bead: Option<&str>,
) -> Result<()> {
    let max_rounds = max_change_rounds.max(1);
    let ts = crate::state::now_iso().replace(['-', ':'], "");
    let thread = match bead {
        Some(id) => format!("plan-{id}"),
        None => format!("plan-{ts}"),
    };
    // review-fix F7: defaults resolve to the spawned flywheel-<base>-<short>
    // names; explicit full names pass through unchanged.
    let planner = resolve_agent_default(project, planner.unwrap_or("oc"));
    let reviewer = resolve_agent_default(project, reviewer.unwrap_or("pi"));
    let implementer = resolve_agent_default(project, implementer.unwrap_or("gk"));
    if planner == reviewer || planner == implementer || reviewer == implementer {
        bail!("plan-review needs three distinct agents (planner/reviewer/implementer)");
    }

    let plan_path = match bead {
        Some(id) => format!(".flywheel/plans/{id}.md"),
        None => format!(".flywheel/plans/plan-{ts}.md"),
    };

    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => bail!("mail backend unavailable for plan-review: {e:#}"),
    };
    let mux = backend::multiplexer_backend(cfg)?;
    let key = project_dir_str(cfg, project)?;
    backend::run_mail(mail.ensure_project(&key))?;

    let mail_target = |agent: &str| resolve_mail_target(agent);
    let subject = format!("[plan-review] {project}");

    eprintln!(
        "plan-review {thread}: planner={} reviewer={} implementer={} max_change_rounds={max_rounds} paper-first",
        planner, reviewer, implementer
    );

    // --- plan ---
    let mut change_rounds = 0u32;
    let mut current_path = plan_path.clone();

    let open = pri_planner_prompt(feature, &current_path);
    mail_send(&*mail, &key, &[mail_target(&planner)], &subject, &open, "high", true, Some(&thread))?;
    settle_agent(&*mux, &planner)?;
    let planner_mail = mail_target(&planner);
    let plan_reply =
        harvest_latest(mail.as_ref(), &key, &thread, &planner_mail)?.unwrap_or_default();
    current_path = extract_plan_path(&plan_reply, &current_path);
    let (_next_phase, action) = pri_after_plan(&current_path);
    let mut phase = PriPhase::AwaitingPlanReview;

    // --- gate loop (review-fix F2) ---
    // A genuine phase-driven state machine. Every iteration routes on the
    // CURRENT phase, performs exactly one agent interaction, and transitions
    // via the pure helpers (pri_route / pri_after_plan / pri_after_implement).
    // `phase` is READ each iteration (no dead assignments). `change_rounds`
    // is bumped by pri_route on CHANGES and checked against max_rounds, so
    // `--max-change-rounds N` is honored for any N (previously the loop ran
    // at most one inline CHANGES cycle then bailed).
    //
    // governed-by: ADR-0001 §9.2 (bd-review-fix-epic-m0b.2)
    let mut action = action;
    loop {
        match phase {
            PriPhase::AwaitingPlan => {
                // planner revision round: prompt, settle, harvest, re-route
                let msg = match &action {
                    PriAction::PromptPlanner { msg } => msg.clone(),
                    _ => bail!("phase AwaitingPlan without PromptPlanner action: {action:?}"),
                };
                mail_send(&*mail, &key, &[mail_target(&planner)], &subject, &msg, "high", true, Some(&thread))?;
                settle_agent(&*mux, &planner)?;
                let pr = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&planner))?
                    .unwrap_or_default();
                current_path = extract_plan_path(&pr, &current_path);
                let (p2, a2) = pri_after_plan(&current_path);
                phase = p2; // AwaitingPlanReview
                action = a2;
            }
            PriPhase::AwaitingPlanReview => {
                // reviewer gate: prompt, settle, harvest, parse, route
                let msg = match &action {
                    PriAction::PromptReviewer { msg } => msg.clone(),
                    _ => {
                        bail!("phase AwaitingPlanReview without PromptReviewer action: {action:?}")
                    }
                };
                mail_send(&*mail, &key, &[mail_target(&reviewer)], &subject, &msg, "high", true, Some(&thread))?;
                settle_agent(&*mux, &reviewer)?;
                let body = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&reviewer))?
                    .unwrap_or_default();
                let token = parse_review_token(&body).ok_or_else(|| {
                    anyhow!(
                        "reviewer reply missing APPROVE|CHANGES:|VERIFIED|REJECT: token: {body}"
                    )
                })?;
                let (p, a, rounds) =
                    pri_route(phase, &token, &current_path, change_rounds, max_rounds);
                phase = p;
                action = a;
                change_rounds = rounds;
            }
            PriPhase::AwaitingImplement => {
                // implementer: prompt (build-slot in prompt), settle, verify
                let msg = match &action {
                    PriAction::PromptImplementer { msg } => msg.clone(),
                    _ => bail!(
                        "phase AwaitingImplement without PromptImplementer action: {action:?}"
                    ),
                };
                mail_send(&*mail, &key, &[mail_target(&implementer)], &subject, &msg, "high", true, Some(&thread))?;
                settle_agent(&*mux, &implementer)?;
                let (p2, a2) = pri_after_implement(&current_path);
                phase = p2; // AwaitingVerify
                action = a2;
            }
            PriPhase::AwaitingVerify => {
                // verify: reviewer checks the implementation
                let msg = match &action {
                    PriAction::PromptReviewer { msg } => msg.clone(),
                    _ => bail!("phase AwaitingVerify without PromptReviewer action: {action:?}"),
                };
                mail_send(&*mail, &key, &[mail_target(&reviewer)], &subject, &msg, "high", true, Some(&thread))?;
                settle_agent(&*mux, &reviewer)?;
                let rev2 = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&reviewer))?
                    .unwrap_or_default();
                let token2 = parse_review_token(&rev2).ok_or_else(|| {
                    anyhow!("verify reply missing VERIFIED|REJECT: token: {rev2}")
                })?;
                let (p3, a3, _) =
                    pri_route(phase, &token2, &current_path, change_rounds, max_rounds);
                phase = p3;
                action = a3;
            }
            PriPhase::Done => {
                println!("plan-review {thread}: VERIFIED (paper-first — human merge/close)");
                return Ok(());
            }
            PriPhase::Escalated => {
                match &action {
                    PriAction::Escalate { reason } => {
                        eprintln!("plan-review {thread}: escalate: {reason}");
                    }
                    other => eprintln!(
                        "plan-review {thread}: escalated with unexpected action {other:?}"
                    ),
                }
                return Ok(());
            }
        }
    }
}

/// Resolve the A/B/judge agents for a debate: explicit names, or default to
/// the swarm's first agents. Returns the (a, b, j) herdr agent names.
/// Resolve the A/B/judge agents for a debate: explicit names, or default to
/// the swarm's first agents. Returns the (a, b, j) herdr agent names.
/// review-fix F7: bare shorts (`oc`/`pi`/`gk`) expand to the spawned
/// `flywheel-<base>-<short>` names; explicit full names pass through.
fn resolve_debate_agents<'a>(
    project: &str,
    agent_a: Option<&'a str>,
    agent_b: Option<&'a str>,
    judge: Option<&'a str>,
) -> Result<(String, String, String)> {
    let a = resolve_agent_default(project, agent_a.unwrap_or("oc"));
    let b = resolve_agent_default(project, agent_b.unwrap_or("pi"));
    let j = resolve_agent_default(project, judge.unwrap_or("gk"));
    if a == b {
        bail!("debate needs two distinct agents (A and B are both '{a}')");
    }
    Ok((a, b, j))
}

/// Wait for an agent to settle (done/idle/blocked) — the turn-finality gate.
fn settle_agent(mux: &dyn backend::MultiplexerBackend, agent: &str) -> Result<()> {
    mux.agent_wait(
        agent,
        &[
            crate::herdr_shim::AgentStatus::Done,
            crate::herdr_shim::AgentStatus::Idle,
            crate::herdr_shim::AgentStatus::Blocked,
        ],
        Some(1_800_000),
    )
    .map_err(|e| anyhow!("agent {agent} settle failed: {e}"))?;
    Ok(())
}

/// Pure extraction of the latest message's body from a `search_messages`
/// envelope. Precedence: `body_md` → `body` → `subject` (mirrors
/// `extract_xproj_from_inbox`). Trims and rejects empty. Returns None when
/// the envelope has no result or no readable text.
///
/// review-fix F1: the debate/plan-review patterns need the reply BODY (the
/// argument / the APPROVE|CHANGES: token), not the subject line.
///
/// governed-by: ADR-0001 §9.1/§9.2 (bd-review-fix-epic-m0b.1)
pub fn extract_latest_body(v: &serde_json::Value) -> Option<String> {
    v.get("result")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|m| {
            m.get("body_md")
                .or_else(|| m.get("body"))
                .or_else(|| m.get("subject"))
                .and_then(serde_json::Value::as_str)
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Drive a surface `send` from sync dispatch with the AM-shaped positional
/// surface (the durable loop's bread-and-butter). Seam: `SendMail` struct,
/// `run_mail` block_on on the shared runtime.
#[allow(clippy::too_many_arguments)]
fn mail_send(
    mail: &dyn backend::MailSurface,
    key: &str,
    to: &[String],
    subject: &str,
    body: &str,
    importance: &str,
    ack: bool,
    thread: Option<&str>,
) -> Result<backend::SendReceipt> {
    Ok(backend::run_mail(mail.send(&backend::SendMail {
        project: key.to_string(),
        sender: mail.agent_name().to_string(),
        to: to.to_vec(),
        subject: subject.to_string(),
        body_md: body.to_string(),
        importance: Some(importance.to_string()),
        ack_required: ack,
        thread_id: thread.map(String::from),
        ..Default::default()
    }))?)
}

/// Harvest the latest message in a thread from a sender (recency-ranked).
fn harvest_latest(
    mail: &dyn backend::MailSurface,
    key: &str,
    thread: &str,
    sender: &str,
) -> Result<Option<String>> {
    let items = backend::run_mail(mail.search(&backend::SearchQuery {
        project: key.to_string(),
        query: String::new(),
        limit: Some(1),
        sender: Some(sender.to_string()),
        thread_id: Some(thread.to_string()),
        ranking: Some("recency".to_string()),
        // review-fix F1: search_messages returns subject-only unless
        // include_body_md is set — the debate/plan-review patterns need
        // the reply BODY (the argument / the APPROVE|CHANGES: token),
        // not the subject line.
        include_body_md: true,
        ..Default::default()
    }))?;
    // body_md first, then body, then subject — mirrors the precedence in
    // extract_xproj_from_inbox so a body-bearing envelope wins over the
    // subject even if a server version omits one of the body keys.
    Ok(items.first().map(|m| {
        if !m.body_md.is_empty() {
            m.body_md.trim().to_string()
        } else {
            m.subject.trim().to_string()
        }
    }))
}

fn cmd_upgrade() -> Result<()> {
    let source = std::env::var("FLYWHEEL_SOURCE").unwrap_or_else(|_| "~/workspace/flywheel".into());
    let expanded = source.replacen(
        "~",
        &dirs::home_dir().unwrap_or_default().display().to_string(),
        1,
    );
    if !std::path::Path::new(&expanded).join("Cargo.toml").exists() {
        bail!("no Cargo.toml at {expanded} — set FLYWHEEL_SOURCE to the repo path");
    }
    println!("installing from {}…", dim(&expanded));
    let status = Command::new("cargo")
        .args(["install", "--path", &expanded, "--force"])
        .status()
        .map_err(|e| anyhow!("cargo not found: {e}"))?;
    if !status.success() {
        bail!("cargo install failed");
    }
    println!("flywheel upgraded");
    Ok(())
}

fn cmd_tutorial() -> Result<()> {
    println!(
        "flywheel — herdr swarm orchestrator\n\n\
         The daily loop (his P1→P8) — one durable command:\n\
         \x20 flywheel spawn <project> --oc=1 --pi=1 --gk=1\n\
         \x20 flywheel loop <project> --queue p1,p2,p4,p5,p6,p8\n\
         \n\
         (kills crash-safe: restart the same command to resume;\n\
         \x20 flywheel loop-status <project> to inspect)\n\
         \n\
         One-shot variant: flywheel workflows run daily <project>\n\
         (define it in {})\n\n\
         Swarm smarts:\n\
         \x20 - flywheel status <project>      states at a glance\n\
         \x20 - flywheel watch <project> --until blocked   alert on blockers\n\
         \x20 - flywheel work triage           bv graph triage\n\
         \x20 - flywheel checkpoint save <project> --msg \"…\"\n\
         \x20 - flywheel resume <project>      re-orient after a break\n\n\
         Coordination:\n\
         \x20 - flywheel mail inbox <project>  agent-mail inbox\n\
         \x20 - flywheel assign <project> --to all --beads bd-1,bd-2\n\
         \x20 - flywheel mail locks <project> --paths src/foo.rs --exclusive\n\n\
         Automation:\n\
         \x20 - flywheel --robot-snapshot     full JSON state\n\
         \x20 - flywheel serve                REST + SSE (port 7337)\n",
        config::config_dir().join("workflows.toml").display()
    );
    Ok(())
}

// ---- durable loop status + internal leaves ----

fn cmd_loop_status(cfg: &Config, project: &str) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let v = rt.block_on(durable::loop_status(cfg, project))?;
    print_json(&v);
    Ok(())
}

/// internal leaf: JSON batch of push events collected within `timeout_ms`.
///
/// governed-by: ADR-0001 (A2 event stream) + ADR-0002 (backend factory).
///
/// **Forward-only:** herdr `events.subscribe` has no since/seq param (schema-
/// verified). This leaf opens a fresh subscription, blocks up to `timeout_ms`,
/// and returns whatever arrived. Crash recovery is NOT via event replay —
/// the loop's `__poll` snapshot is the correctness floor. `after_seq` is
/// best-effort in-run dedup only (skips events whose local seq is <= it).
///
/// Wire shape:
/// ```json
/// { "events": [{ "event_type", "pane_id", "agent", "status", "seq" }, ...],
///   "max_seq": 0, "stream_error": null }
/// ```
fn cmd_internal_poll_events(
    cfg: &Config,
    project: &str,
    timeout_ms: u64,
    after_seq: Option<u64>,
) -> Result<()> {
    let batch = collect_poll_events(cfg, project, timeout_ms, after_seq)?;
    print_json(&batch);
    Ok(())
}

/// Collect status-change events for `project` until `timeout_ms` elapses.
/// Extracted for unit-testability of the empty/error paths via the JSON shape.
fn collect_poll_events(
    cfg: &Config,
    project: &str,
    timeout_ms: u64,
    after_seq: Option<u64>,
) -> Result<serde_json::Value> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = match find_workspace_dyn(&*backend, project) {
        Ok(id) => id,
        Err(e) => {
            // no workspace yet — empty batch, not a hard error (loop may
            // spawn agents later; heartbeat __poll handles empty swarms)
            return Ok(json!({
                "events": [],
                "max_seq": after_seq.unwrap_or(0),
                "stream_error": format!("no workspace: {e}"),
            }));
        }
    };

    let panes = backend.pane_list(Some(&ws_id)).unwrap_or_default();
    if panes.is_empty() {
        return Ok(json!({
            "events": [],
            "max_seq": after_seq.unwrap_or(0),
            "stream_error": null,
        }));
    }

    let subs: Vec<Subscription> = panes
        .iter()
        .map(|p| Subscription::agent_status_changed(p.pane_id.clone()))
        .collect();

    let mut stream = match backend.subscribe(&subs) {
        Ok(s) => s,
        Err(e) => {
            return Ok(json!({
                "events": [],
                "max_seq": after_seq.unwrap_or(0),
                "stream_error": format!("subscribe failed: {e}"),
            }));
        }
    };

    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut seq = after_seq.unwrap_or(0);
    let mut stream_error: Option<String> = None;

    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        // floor: don't spin with 0-duration timeouts
        let read_to = remaining.max(Duration::from_millis(1));
        if let Err(e) = stream.set_read_timeout(Some(read_to)) {
            stream_error = Some(format!("set_read_timeout: {e}"));
            break;
        }
        match stream.read_event() {
            Ok(Some(ev)) => {
                if let Some(item) = summarize_status_event(&ev, &mut seq) {
                    if after_seq.is_some_and(|a| item.seq <= a) {
                        continue; // already seen this run
                    }
                    events.push(json!({
                        "event_type": item.event_type,
                        "pane_id": item.pane_id,
                        "agent": item.agent,
                        "status": item.status,
                        "seq": item.seq,
                    }));
                }
            }
            Ok(None) => {
                stream_error = Some("stream closed".into());
                break;
            }
            Err(_) => {
                // read timeout or transient I/O — end this batch (leaf returns
                // whatever was collected; next iteration opens a fresh stream)
                break;
            }
        }
    }

    Ok(json!({
        "events": events,
        "max_seq": seq,
        "stream_error": stream_error,
    }))
}

/// Compact form of a status-change push for the `__poll_events` JSON batch.
struct StatusEventItem {
    event_type: &'static str,
    pane_id: String,
    agent: Option<String>,
    status: String,
    seq: u64,
}

/// Map a typed backend push event to a status item, advancing `seq`. Returns
/// `None` for non-status events (defensive — the subscription is already
/// narrowed to agent-status pushes). Consumes the UNIFIED toron-core
/// `BackendEvent` (ADR-0005 P1); the wire event_type strings are preserved so
/// downstream consumers see the same names as before the trait unification.
fn summarize_status_event(
    ev: &toron_core::backend::BackendEvent,
    seq: &mut u64,
) -> Option<StatusEventItem> {
    use toron_core::backend::BackendEvent::*;
    let (event_type, pane_id, agent, status) = match ev {
        AgentStatusChanged { pane, agent, status } => (
            "pane.agent_status_changed",
            pane.0.clone(),
            agent.clone(),
            status.as_str().to_string(),
        ),
        AgentDetected { pane, agent, status } => (
            "pane.agent_detected",
            pane.0.clone(),
            agent.clone(),
            status.as_str().to_string(),
        ),
        PaneExited { pane } => ("pane.exited", pane.0.clone(), None, "exited".to_string()),
        Raw(_) => return None,
    };
    *seq = seq.saturating_add(1);
    Some(StatusEventItem {
        event_type,
        pane_id,
        agent,
        status,
        seq: *seq,
    })
}

/// internal leaf: JSON [[name, status], ...] for a project's managed agents.
///
/// governed-by: ADR-0002 — constructs the multiplexer via the factory so the
/// durable loop is backend-agnostic (herdr today; superlogical/toron later).
fn cmd_internal_poll(cfg: &Config, project: &str, to: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let agents = backend
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect::<Vec<_>>();
    // [name, status, kind, pane_id] — pane_id enables event→name routing
    // (bd-event-dispatch-zero-latency). Older parsers ignore the 4th element.
    let managed: Vec<Value> = if to == "all" {
        agents
            .iter()
            .map(|a| {
                json!([
                    agent_target(a),
                    a.agent_status.as_str(),
                    a.agent.clone().unwrap_or_default(),
                    a.pane_id,
                ])
            })
            .collect()
    } else {
        let resolved = resolve_agents_dyn(&*backend, &ws_id, project, to)?;
        resolved
            .iter()
            .map(|a| {
                json!([
                    agent_target(a),
                    a.agent_status.as_str(),
                    a.agent.clone().unwrap_or_default(),
                    a.pane_id,
                ])
            })
            .collect()
    };
    print_json(&json!({ "agents": managed }));
    Ok(())
}

/// internal leaf: send one prompt to a named agent.
///
/// **ADR-0002 note:** this leaf is NOT yet migrated to the trait factory. The
/// confirm-path uses `Client::agent_prompt_coded` (typed `HerdrError` code →
/// `classify_send_result` landing semantics), which the trait surface doesn't
/// expose (the trait's `agent_prompt` returns a minimal JSON, losing the coded
/// error). Migrating requires either a trait-level coded-prompt method or a
/// different landing-detection mechanism — a design question, deferred to a
/// follow-up bead. The `__poll` and `__mail-blocked` leaves ARE migrated
/// (they map cleanly onto trait methods).
///
/// Without `--confirm`: fire-and-forget (`wait: None`) — byte-identical to
/// the legacy path. With `--confirm [TIMEOUT_MS]`: wait for the prompt to
/// LAND (agent reaches working|blocked), classify the herdr error code, and
/// emit `{"ok": true, "name", "landed", "status", "reason"}`. Exit 1 only
/// for hard errors (agent not found); landing failures exit 0 with flags.
fn cmd_internal_send(
    cfg: &Config,
    project: &str,
    name: &str,
    text: &str,
    confirm_ms: Option<u64>,
    invocation: Option<&str>,
    iteration: Option<u64>,
) -> Result<()> {
    // resume-dedup (bd-resume-dedup): a landed record for THIS invocation
    // means the prompt was delivered before a crash — skip the herdr call
    // entirely. The __send subprocess is the idempotency boundary: it runs
    // FRESH at send time, so the check cannot go stale (a workflow-level
    // check's result would itself be checkpointed and replayed after the
    // crash window — the folded form is the correct one; documented in the
    // bead close). Skipped sends emit the landed shape with deduped:true and
    // do NOT write a new record (the delivery is already recorded).
    if let Some(inv) = invocation.filter(|i| !i.is_empty()) {
        let hash = state::fnv1a64(text);
        let records = state::load_last_sent(project);
        let record = state::find_last_sent(&records, name, hash);
        if !durable::should_resend(record, inv) {
            print_json(&json!({
                "ok": true, "name": name, "landed": true,
                "status": serde_json::Value::Null, "reason": "landed", "deduped": true,
            }));
            return Ok(());
        }
    }

    // review-fix F6: __send routes through the backend factory (ADR-0002).
    // The trait now exposes agent_prompt_coded with landing data, so the
    // durable loop's dispatch leaf is backend-agnostic (herdr today; toron /
    // superlogical later) with byte-identical landing classification.
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let agent = backend
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
    let target = agent_target(&agent).to_string();

    // last-sent registry: record what was delivered (or attempted). Best-
    // effort — a registry failure must never lose the send itself.
    // cg6.5: `ambiguous` marks timeout-confirm sends — the prompt may have
    // started, so resume-dedup (should_resend) must NOT re-delegate them.
    let record = |landed: bool, ambiguous: bool| {
        let rec = state::SentRecord {
            name: name.to_string(),
            text_hash: state::fnv1a64(text),
            invocation: invocation.unwrap_or_default().to_string(),
            iteration: iteration.unwrap_or(0),
            landed,
            ambiguous,
        };
        if let Err(e) = state::record_sent(project, rec) {
            eprintln!("flywheel: cannot record sent prompt: {e:#}");
        }
    };

    let Some(timeout_ms) = confirm_ms else {
        // legacy fire-and-forget path — a successful herdr call means the
        // prompt was written to the agent's terminal
        backend.prompt(&target, text, None)?;
        record(true, false);
        print_json(&json!({ "ok": true, "name": name }));
        return Ok(());
    };

    let wait = WaitOptions {
        timeout_ms: Some(timeout_ms),
        // landing semantics: the prompt started executing. NOT idle/done —
        // a long task must not hold the leaf for its full duration.
        until: vec![AgentStatus::Working, AgentStatus::Blocked],
    };
    // review-fix F6: PromptLanding carries status XOR code; classify
    // identically to the concrete path (landed → status, else code).
    let landing = backend.prompt_coded(&target, text, Some(&wait))?;
    let verdict = classify_send_result(landing.code.as_deref(), landing.status);
    let (landed, status, reason) = match verdict {
        SendVerdict::Landed(st) => (
            true,
            st.map(|s| s.as_str().to_string()),
            "landed".to_string(),
        ),
        SendVerdict::Stalled => (false, None, "stalled".to_string()),
        SendVerdict::TimedOut => (false, None, "timeout".to_string()),
        SendVerdict::Coded(code) => (false, None, format!("error:{code}")),
    };
    // cg6.5: a timeout is AMBIGUOUS (the prompt may have started) — record it
    // as such so resume never re-delegates a possibly-mid-flight prompt.
    record(landed, reason == "timeout");
    print_json(&json!({
        "ok": true, "name": name, "landed": landed,
        "status": status, "reason": reason,
    }));
    Ok(())
}

/// internal leaf: ready beads as [{id, title}] via `br ready --json`.
/// Resolve a `__wait-output` target to a pane id: an agent NAME (matched in
/// the caller-provided, workspace-filtered agent list) maps to its pane; a
/// target containing `:` is treated as a raw pane id; anything else is
/// unknown. Pure — no lookup logic duplicated from `__send`.
fn resolve_pane_id(agents: &[herdr_shim::AgentInfo], target: &str) -> Option<String> {
    if let Some(a) = agents.iter().find(|a| a.name.as_deref() == Some(target)) {
        return Some(a.pane_id.clone());
    }
    if target.contains(':') && !target.is_empty() {
        return Some(target.to_string());
    }
    None
}

/// The wait-output mode for the `--regex` flag: herdr's literal-substring
/// mode is spelled `match` in the leaf (mapped to `substring` in the client).
fn wait_output_mode(regex: bool) -> &'static str {
    if regex {
        "regex"
    } else {
        "match"
    }
}

/// The leaf's machine-readable output shape.
fn wait_output_json(matched: bool, matched_line: Option<&str>, pane_id: &str) -> Value {
    json!({
        "matched": matched,
        "matched_line": matched_line,
        "pane_id": pane_id,
    })
}

/// internal leaf: block until a pane's output matches a pattern.
///
/// Target is an agent name (resolved via the project's agent list) or a raw
/// pane id. Exit 0 on match, 1 on timeout / unknown target — stdout carries
/// `{"matched", "matched_line", "pane_id"}` either way (stderr for messages).
/// For automation/scripts; the durable workflow does not use this leaf (no
/// workflow-step type yet — future option, see cmd_workflow).
fn cmd_internal_wait_output(
    cfg: &Config,
    project: &str,
    target: &str,
    pattern: &str,
    regex: bool,
    timeout_ms: Option<u64>,
) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agents = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect::<Vec<_>>();
    let Some(pane_id) = resolve_pane_id(&agents, target) else {
        let out = wait_output_json(false, None, "");
        print_json(&out);
        eprintln!("flywheel: target {target} not found in {project}");
        std::process::exit(1);
    };
    match herdr_shim::pane_wait_output(
        &mut c,
        &pane_id,
        wait_output_mode(regex),
        pattern,
        timeout_ms,
    ) {
        Ok(r) => {
            print_json(&wait_output_json(
                true,
                r.matched_line.as_deref(),
                &r.pane_id,
            ));
            Ok(())
        }
        Err(e) if e.code == "timeout" => {
            let out = wait_output_json(false, None, &pane_id);
            print_json(&out);
            eprintln!("flywheel: no match for {pattern:?} in {pane_id} within timeout");
            std::process::exit(1);
        }
        Err(e) => Err(anyhow::anyhow!("pane.wait_for_output failed: {e:#}")),
    }
}

fn cmd_internal_ready(cfg: &Config, project: &str) -> Result<()> {
    let dir = project_dir(cfg, project);
    let out = std::process::Command::new("br")
        .args(["ready", "--json"])
        .current_dir(&dir)
        .output()
        .map_err(|e| anyhow!("br not found: {e}"))?;
    if !out.status.success() {
        bail!(
            "br ready failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: Value =
        serde_json::from_str(raw.trim()).map_err(|e| anyhow!("br ready bad JSON: {e}: {raw}"))?;
    let beads: Vec<Value> = v
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|b| {
            json!({
                "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                "title": b.get("title").and_then(Value::as_str).unwrap_or("")
            })
        })
        .collect();
    print_json(&json!({ "beads": beads }));
    Ok(())
}

/// internal leaf: in-progress beads as [{id, title}] via `br list --status in_progress`.
fn cmd_internal_inprogress(cfg: &Config, project: &str) -> Result<()> {
    let dir = project_dir(cfg, project);
    let out = std::process::Command::new("br")
        .args(["list", "--status", "in_progress", "--json"])
        .current_dir(&dir)
        .output()
        .map_err(|e| anyhow!("br not found: {e}"))?;
    if !out.status.success() {
        bail!(
            "br list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: Value =
        serde_json::from_str(raw.trim()).map_err(|e| anyhow!("br list bad JSON: {e}: {raw}"))?;
    // br list --json wraps issues under "issues"
    let list: &Vec<Value> = v
        .get("issues")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .ok_or_else(|| anyhow!("br list output has no issues array: {raw}"))?;
    let beads: Vec<Value> = list
        .iter()
        .map(|b| {
            json!({
                "id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                "title": b.get("title").and_then(Value::as_str).unwrap_or("")
            })
        })
        .collect();
    print_json(&json!({ "beads": beads }));
    Ok(())
}

/// internal leaf: print the herdr agent kind of a named agent.
fn cmd_internal_kind(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let agent = backend
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
    println!("{}", agent.agent.as_deref().unwrap_or("unknown"));
    Ok(())
}

/// internal leaf: print recent output of a named agent (context-marker check).
/// review-fix F6: routes through the backend factory (ADR-0002).
fn cmd_internal_read(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let agent = backend
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
    let target = agent_target(&agent).to_string();
    let text = backend
        .agent_read(&target, "recent", 60)?
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    // state-file fallback channel: parse the agent's state file when present.
    // Malformed/absent files yield null — never an error (the loop treats
    // null as "terminal channel only", identical to pre-zwa behavior).
    let state: Option<agentstate::AgentStateFile> =
        agentstate::state_file_path(&project_dir(cfg, project), &target)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|content| agentstate::parse_state_file(&content));
    print_json(&json!({ "terminal": text, "state": state }));
    Ok(())
}

/// internal leaf: close the agent's pane, split a fresh pane, restart the agent
/// under the same name (fresh session context).
fn cmd_internal_respawn(
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
    force: bool,
) -> Result<()> {
    // review-fix F6: respawn routes through the backend factory (ADR-0002).
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let dir = project_dir(cfg, project);
    // The agent record may already be gone (herdr drops dead agents ~0.5s
    // after they stop responding — the escalation path respawns exactly
    // those). The pane itself usually survives; split from any pane in the
    // workspace and skip closing an old pane we cannot identify.
    let agents = backend.agent_list()?;
    // cg6.3: replay-idempotency guard — if an agent with this name already
    // exists in this workspace with a HEALTHY status, the respawn already
    // happened (crash-window replay of the same leaf, or a manual double
    // call): no-op success instead of spawning a second twin. The
    // context_limit carve-out (`--force`): an idle-but-exhausted agent is
    // healthy and must still be respawned — its replay window (crash between
    // the guard and the spawn) is a documented at-least-once residual.
    if !force
        && let Some(a) = agents.iter().find(|a| {
            a.workspace_id == ws_id && a.name.as_deref() == Some(name)
        })
    {
        let healthy = matches!(a.agent_status.as_str(), "idle" | "working" | "blocked");
        if healthy {
            print_json(&json!({
                "already_exists": true,
                "name": name,
                "pane_id": a.pane_id,
            }));
            return Ok(());
        }
    }
    let agent = agents
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name));
    let old_pane = agent.as_ref().map(|a| a.pane_id.clone());
    let panes = backend.pane_list(Some(&ws_id))?;
    let anchor = old_pane
        .as_ref()
        .and_then(|old| {
            panes
                .iter()
                .find(|p| &p.pane_id != old)
                .map(|p| p.pane_id.clone())
        })
        .or_else(|| panes.first().map(|p| p.pane_id.clone()))
        .ok_or_else(|| anyhow!("no pane in {project} to split for respawn"))?;
    let new_pane = backend.pane_split(&anchor, "down", Some(&utf8_path(&dir, "workspace dir")?))?;
    if let Some(old) = &old_pane {
        // trait has no pane_close default; herdr Client has no typed helper —
        // best-effort close via the concrete client only when we have one.
        if let Ok(mut c) = Client::connect(Some(&cfg.socket_path())) {
            let _ = c.call("pane.close", json!({ "pane_id": old }));
        }
    }
    let fresh = start_agent_with_retry_dyn(&*backend, name, kind, &new_pane.pane_id, &[])?;
    // pane-identity bridge bookkeeping (best-effort — never fail the respawn
    // on mail trouble): the old pane died (its identity file is stale), the
    // fresh pane must resolve to the same name (the agent keeps its identity)
    if let Ok(mail) = backend::mail_backend(cfg) && let Some(key) = dir.to_str() {
        if let Some(old) = &old_pane && let Err(e) = backend::run_mail(mail.clear_pane_identity(key, old)) {
            eprintln!("flywheel: mail clear_pane_identity failed for {old}: {e}");
        }
        if let Err(e) = backend::run_mail(mail.write_pane_identity(key, &fresh.pane_id, &agent_mail_name(name))) {
            eprintln!(
                "flywheel: mail write_pane_identity failed for {name} on {}: {e}",
                fresh.pane_id
            );
        }
    }
    println!(
        "respawned {} {} -> {}",
        kind_paint(kind),
        old_pane.as_deref().unwrap_or("<gone>"),
        fresh.pane_id
    );
    Ok(())
}

/// internal leaf: scan orchestrator inbox for `[xproj]` swarm requests.
/// Soft-fail: missing mail → empty list. Output: `{ "items": [ XprojInboxItem ] }`.
fn cmd_internal_xproj_inbox(cfg: &Config, project: &str, limit: u32) -> Result<()> {
    // review-fix F6: the xproj-inbox leaf routes through the backend factory.
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(_) => {
            print_json(&json!({ "items": [] }));
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&json!({ "items": [] }));
        return Ok(());
    }
    let name = mail.agent_name().to_string();
    let v = match backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key.clone(),
        agent: name,
        unread_only: false,
        urgent_only: false,
        include_bodies: true,
        limit: Some(limit as usize),
        since_ts: None,
    })) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("flywheel: xproj-inbox fetch failed: {e:#}");
            print_json(&json!({ "items": [] }));
            return Ok(());
        }
    };
    let v_json = serde_json::to_value(&v).unwrap_or_default();
    let items = extract_xproj_from_inbox(&v_json);
    print_json(&json!({ "items": items }));
    Ok(())
}

/// internal leaf: spawn the swarm for a parsed `[xproj]` request (full loop
/// branch — bd-pattern-cross-project-swarm §9.5).
///
/// Called DIRECTLY by the durable loop (not a queued agent prompt): parse the
/// request's `recipe`/`workers`, spawn the swarm in THIS project, and reply a
/// "swarm started" ack in the request thread so the requester sees it.
///
/// Best-effort: spawn failure replies an error in-thread (never kills the
/// loop); a missing project dir or unknown recipe is surfaced in-thread too.
fn cmd_internal_xproj_spawn(
    cfg: &Config,
    project: &str,
    message_id: &str,
    thread_id: &str,
    from: &str,
    recipe: &str,
    workers: u32,
) -> Result<()> {
    let dir = project_dir(cfg, project);
    let outcome = if !dir.exists() {
        Err(anyhow!("project dir missing: {}", dir.display()))
    } else {
        // workers param = opencode count by default; a recipe with its own
        // oc/pi/gk counts wins (recipe_counts prefers the recipe).
        cmd_spawn(
            cfg,
            project,
            &AgentCounts {
                oc: Some(workers.max(1)),
                pi: None,
                gk: None,
            },
            Some(recipe),
            &[],
            None,
            true,  // background workspace (no_focus — must not steal terminal focus)
            false, // not peer-aware by default
        )
    };
    let spawned_ok = outcome.is_ok();
    // reply in the request thread (best-effort; mail down must not kill the loop)
    let reply = match outcome {
        Ok(_) => format!(
            "[xproj] swarm started: recipe={recipe}, {workers} worker(s) in project {project} (msg {message_id}). Reply thread: {thread_id}."
        ),
        Err(e) => format!(
            "[xproj] spawn FAILED for msg {message_id}: {e:#}. Requester {from}, recipe={recipe}, workers={workers}."
        ),
    };
    if let Ok(mail) = backend::mail_backend(cfg) {
        let key = project_dir(cfg, project)
            .to_str()
            .unwrap_or(project)
            .to_string();
        let _ = backend::run_mail(mail.ensure_project(&key));
        let _ = mail_send(
            &*mail,
            &key,
            &[resolve_mail_target(from)],
            &format!("[xproj] swarm reply (msg {message_id})"),
            &reply,
            "normal",
            false,
            Some(thread_id),
        );
    }
    print_json(&json!({
        "ok": true,
        "message_id": message_id,
        "spawned": spawned_ok,
        "project": project,
        "recipe": recipe,
        "workers": workers,
    }));
    Ok(())
}

/// internal leaf: notify a blocked agent via agent-mail (durable workflow leaf).
///
/// governed-by: ADR-0002 — constructs the mail backend via the factory so the
/// durable loop is backend-agnostic (agent-mail MCP today; toron later).
///
/// Best-effort by contract: a missing/unreachable agent-mail must NEVER kill
/// an overnight loop (same standard as the lease-renewal heartbeat) — the
/// leaf exits 0 with `{"ok": false, "error": ...}` and the next poll retries.
/// internal leaf: notify a blocked agent via agent-mail, tiered
/// (bd-blocked-escalation-protocol). Tier 0 = first ping (high + ack),
/// 1 = reply high in-thread, 2 = urgent + thread summary, 3 = halt.
///
/// governed-by: ADR-0002 — constructs the mail backend via the factory so the
/// durable loop is backend-agnostic (agent-mail MCP today; toron later).
///
/// Best-effort by contract: a missing/unreachable agent-mail must NEVER kill
/// an overnight loop (same standard as the lease-renewal heartbeat) — the
/// leaf exits 0 with `{"ok": false, "error": ...}` and the next poll retries.
fn cmd_internal_mail_blocked(cfg: &Config, project: &str, name: &str, tier: u32) -> Result<()> {
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for blocked alert to {name}: {e:#}");
            print_json(&json!({ "ok": false, "notified": name, "error": format!("{e:#}") }));
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    // the recipient is the agent's REGISTERED agent-mail identity (bd-odc) —
    // `name` is the herdr name, which is not a registrable identity
    let mail_target = resolve_mail_target(name);
    let thread = format!("health-{name}");
    let out = (|| -> Result<backend::SendReceipt> {
        backend::run_mail(mail.ensure_project(&key)).map_err(|e| anyhow!("{e}"))?;
        match tier {
        0 => mail_send(
            &*mail,
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] you are blocked",
            "The swarm loop detected you are blocked. Resolve or report the blocker.",
            "high",
            true,
            Some(&thread),
        ),
        1 => mail_send(
            &*mail,
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] still blocked — 2nd ping (high)",
            "Still blocked after a full interval. Please resolve or report the blocker so the loop can continue.",
            "high",
            true,
            Some(&thread),
        ),
        2 => mail_send(
            &*mail,
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] URGENT: blocked — escalation",
            "Still blocked after multiple pings. Escalating; human attention requested.",
            "urgent",
            true,
            Some(&thread),
        ),
        _ => mail_send(
            &*mail,
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] HALTS: blocked too long — human required",
            "The swarm loop is halting because this agent stayed blocked past the escalation budget. Resolve the blocker, then re-run the loop.",
            "urgent",
            true,
            Some(&thread),
        ),
        }
    })();
    match out {
        Ok(_) => {
            print_json(&json!({ "ok": true, "notified": mail_target, "tier": tier }));
            Ok(())
        }
        Err(e) => {
            eprintln!("flywheel: blocked alert to {name} failed: {e:#}");
            print_json(&json!({ "ok": false, "notified": mail_target, "error": format!("{e:#}") }));
            Ok(())
        }
    }
}

/// internal leaf: diagnose an unresponsive agent (bd-event-self-heal).
///
/// 1. Capture recent output via the read path (best-effort).
/// 2. Send high-importance mail on thread `health-<name>` to the orchestrator
///    identity (config agent_mail.name) so a human/supervisor can act.
///
/// Soft-fail always: exit 0 with `{ok, ...}` so overnight loops never die.
fn cmd_internal_diagnose(cfg: &Config, project: &str, name: &str, reason: &str) -> Result<()> {
    // --- tail (best-effort) ---
    let mut tail = String::new();
    let backend = match backend::multiplexer_backend(cfg) {
        Ok(b) => b,
        Err(e) => {
            print_json(&json!({
                "ok": false,
                "name": name,
                "reason": reason,
                "error": format!("multiplexer unavailable: {e:#}"),
            }));
            return Ok(());
        }
    };
    // Prefer agent_read (recent, 80 lines); soft on failure.
    match backend.agent_read(name, "recent", 80) {
        Ok(v) => {
            // herdr returns a string or {text}/array — normalize
            tail = v
                .as_str()
                .map(str::to_string)
                .or_else(|| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_else(|| v.to_string());
            if tail.len() > 4000 {
                let start = tail.len().saturating_sub(4000);
                tail = tail[start..].to_string();
            }
        }
        Err(e) => {
            eprintln!("flywheel: diagnose read {name}: {e:#}");
        }
    }

    // --- health mail ---
    let mut mailed = false;
    let mut mail_err: Option<String> = None;
    let recipient = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "Flywheel".to_string());
    match backend::mail_backend(cfg) {
        Ok(mail) => {
            let key = project_dir_str(cfg, project)?;
            let subject = format!("[{project}] {name} unresponsive ({reason})");
            let body = format!(
                "Agent **{name}** is unresponsive (`{reason}`).\n\n\
                 Project: `{project}`\n\
                 Thread: `health-{name}`\n\n\
                 ### Recent output (truncated)\n```\n{tail}\n```\n\n\
                 flywheel will attempt respawn if the agent stays dead (budgeted)."
            );
            match (|| -> Result<backend::SendReceipt> {
                backend::run_mail(mail.ensure_project(&key)).map_err(|e| anyhow!("{e}"))?;
                mail_send(
                    &*mail,
                    &key,
                    std::slice::from_ref(&recipient),
                    &subject,
                    &body,
                    "high",
                    false,
                    Some(&format!("health-{name}")),
                )
            })() {
                Ok(_) => mailed = true,
                Err(e) => {
                    mail_err = Some(format!("{e:#}"));
                    eprintln!("flywheel: diagnose mail for {name}: {e:#}");
                }
            }
        }
        Err(e) => {
            mail_err = Some(format!("mail backend: {e:#}"));
        }
    }

    print_json(&json!({
        "ok": true,
        "name": name,
        "reason": reason,
        "tail_len": tail.len(),
        "mailed": mailed,
        "recipient": recipient,
        "mail_error": mail_err,
    }));
    Ok(())
}

/// Internal leaf command: renew this identity's active file reservations in
/// the project, extending each by `extend_seconds` (bd-mail-lease-renewal-
/// durable). Best-effort from the workflow's perspective — the durable leaf
/// treats a non-zero exit as a missed renew (log + continue), never a hard
/// loop failure. Output: `{"ok": true, "renewed": N, "file_reservations": [...]}`.
fn cmd_internal_renew_leases(cfg: &Config, project: &str, extend_seconds: u64) -> Result<()> {
    let mail = backend::mail_backend(cfg)?;
    let key = project_dir_str(cfg, project)?;
    backend::run_mail(mail.ensure_project(&key))?;
    // NOTE: MailSurface::agent_name() is the orchestrator identity; the
    // reported `agent` must match the reservations' owner — use the
    // configured name explicitly.
    let agent = orchestrator_mail_name(cfg);
    let v = backend::run_mail(mail.renew_reservations(&key, &agent, &[], extend_seconds))
        .map_err(|e| anyhow!("mail backend: {e}"))?;
    // Surface `RenewResult` → the AM-parity `file_reservations` envelope
    // `list_own_reservations` / older CLI consumers parse.
    let envelope = json!({
        "renewed": v.renewed,
        "file_reservations": v.reservations.iter().map(|r| json!({
            "id": r.id,
            "path_pattern": r.path_pattern,
            "old_expires_ts": "",
            "new_expires_ts": r.expires_ts_us.map(|u| u.to_string()).unwrap_or_default(),
        })).collect::<Vec<_>>(),
    });
    let reservations = crate::mail_shim::list_own_reservations(&envelope);
    print_json(&json!({
        "ok": true,
        "renewed": v.renewed,
        "agent": agent,
        "file_reservations": reservations,
    }));
    Ok(())
}

// ---- serve (feature: serve) ----

#[cfg(feature = "serve")]
fn cmd_serve(cfg: &Config, port: u16) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve_async(cfg.clone(), port))
}

#[cfg(feature = "serve")]
async fn serve_async(cfg: config::Config, port: u16) -> Result<()> {
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::Arc;

    #[derive(Clone)]
    struct AppState {
        cfg: config::Config,
    }

    async fn health() -> &'static str {
        "{\"status\":\"ok\"}"
    }

    async fn snapshot(State(st): State<Arc<AppState>>) -> Result<String, StatusCode> {
        let mut c = Client::connect(Some(&st.cfg.socket_path()))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let agents = c
            .agent_list()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        Ok(serde_json::to_string(&json!({
            "agents": agents.iter().map(|a| json!({
                "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                "workspace_id": a.workspace_id, "pane_id": a.pane_id
            })).collect::<Vec<_>>()
        }))
        .unwrap())
    }

    async fn send(State(st): State<Arc<AppState>>, body: String) -> Result<String, StatusCode> {
        let req: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
        let project = req["project"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        let to = req["to"].as_str().unwrap_or("all");
        let msg = req["msg"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
        // same destructive-command guard as `flywheel send` — the HTTP surface
        // must not bypass the policy
        let policy = config::Policy::load().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if policy.violation(msg).is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        let mut c = Client::connect(Some(&st.cfg.socket_path()))
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let ws_id = find_workspace(&mut c, project).map_err(|_| StatusCode::NOT_FOUND)?;
        let targets =
            resolve_agents(&mut c, &ws_id, project, to).map_err(|_| StatusCode::BAD_REQUEST)?;
        for a in &targets {
            c.agent_prompt(agent_target(a), msg, None)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        }
        Ok(json!({ "ok": true, "targets": targets.len() }).to_string())
    }

    async fn sse_handler(State(st): State<Arc<AppState>>) -> axum::response::Response {
        use axum::response::sse::Event;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
        let cfg = st.cfg.clone();
        tokio::spawn(async move {
            let mut tick = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let mut c = match Client::connect(Some(&cfg.socket_path())) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let agents = c.agent_list().unwrap_or_default();
                let payload = json!({
                    "tick": tick, "ts": chrono_like_ts(),
                    "agents": agents.iter().map(|a| json!({
                        "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
                        "workspace_id": a.workspace_id
                    })).collect::<Vec<_>>()
                })
                .to_string();
                tick += 1;
                if tx
                    .send(Ok(Event::default().event("state").data(payload)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        use axum::response::IntoResponse;
        axum::response::Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
            .keep_alive(
                axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
            )
            .into_response()
    }

    let st = Arc::new(AppState { cfg: cfg.clone() });
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/send", post(send))
        .route("/events", get(sse_handler))
        .route("/openapi.json", get(|| async {
            json!({
                "openapi": "3.0.0",
                "info": { "title": "flywheel", "version": VERSION },
                "paths": {
                    "/health": { "get": { "summary": "health" } },
                    "/api/v1/snapshot": { "get": { "summary": "full agent state" } },
                    "/api/v1/send": { "post": { "summary": "send prompt", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "project": {"type":"string"}, "to": {"type":"string"}, "msg": {"type":"string"} } } } } } } },
                    "/events": { "get": { "summary": "SSE state stream" } }
                }
            })
            .to_string()
        }))
        .with_state(st);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("flywheel serve on http://127.0.0.1:{port} (/health /api/v1/snapshot /api/v1/send /events /openapi.json)");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(not(feature = "serve"))]
fn cmd_serve(_cfg: &Config, _port: u16) -> Result<()> {
    bail!("serve support not compiled in (build with --features serve)")
}

#[cfg(test)]
mod wait_output_tests {
    use super::*;
    use crate::herdr_shim::{AgentInfo, PaneWaitResult};

    fn agent(name: Option<&str>, pane: &str) -> AgentInfo {
        AgentInfo {
            agent: Some("opencode".into()),
            agent_status: AgentStatus::Idle,
            cwd: Some("/proj".into()),
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: name.map(String::from),
            pane_id: pane.to_string(),
            revision: 1,
            state_change_seq: 0,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term_1".into(),
            terminal_title: None,
            workspace_id: "w1".into(),
        }
    }

    #[test]
    fn resolve_pane_id_maps_agent_name_to_pane() {
        let agents = vec![agent(Some("flywheel-x-oc"), "w1:p1")];
        assert_eq!(
            resolve_pane_id(&agents, "flywheel-x-oc"),
            Some("w1:p1".to_string())
        );
    }

    #[test]
    fn resolve_pane_id_accepts_raw_pane_id() {
        let agents = vec![agent(Some("flywheel-x-oc"), "w1:p1")];
        assert_eq!(resolve_pane_id(&agents, "w2:p3"), Some("w2:p3".to_string()));
    }

    #[test]
    fn resolve_pane_id_unknown_target_is_none() {
        let agents = vec![agent(Some("flywheel-x-oc"), "w1:p1")];
        assert_eq!(resolve_pane_id(&agents, "no-such-agent"), None);
        assert_eq!(resolve_pane_id(&agents, ""), None);
    }

    #[test]
    fn wait_output_mode_maps_regex_flag() {
        assert_eq!(wait_output_mode(true), "regex");
        assert_eq!(wait_output_mode(false), "match");
    }

    #[test]
    fn wait_output_json_shape() {
        let hit = wait_output_json(true, Some("tests passed"), "w1:p1");
        assert_eq!(hit["matched"], true);
        assert_eq!(hit["matched_line"], "tests passed");
        assert_eq!(hit["pane_id"], "w1:p1");
        let miss = wait_output_json(false, None, "w1:p1");
        assert_eq!(miss["matched"], false);
        assert_eq!(miss["matched_line"], serde_json::Value::Null);
    }

    #[test]
    fn pane_wait_result_json_uses_leaf_shape() {
        // the leaf renders a PaneWaitResult via the same helper
        let r = PaneWaitResult {
            pane_id: "w1:p1".into(),
            matched_line: Some("ready".into()),
            read: String::new(),
        };
        let v = wait_output_json(true, r.matched_line.as_deref(), &r.pane_id);
        assert_eq!(v["matched_line"], "ready");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn render_produces_subagent_with_frontmatter_and_body() {
        let out = render_integration("opencode", "9.9.9", "2026-08-04T00:00:00Z").unwrap();
        // subagent shape: YAML frontmatter naming the beads agent
        assert!(out.starts_with("---\n"));
        assert!(out.contains("name: beads"));
        // version + generated stamps in the frontmatter comment
        assert!(out.contains("flywheel 9.9.9"));
        assert!(out.contains("2026-08-04T00:00:00Z"));
        // the shared body's lifecycle
        assert!(out.contains("br update bd-<id> --claim"));
        assert!(out.contains("CONTEXT_LIMIT"));
        assert!(out.contains(".flywheel/agent-state/<AGENT_NAME>.json"));
        // per-kind config note
        assert!(out.contains("~/.config/opencode"));
        // no leftovers
        assert!(!out.contains("{{BODY}}"));
        assert!(!out.contains("{{KIND_NOTES}}"));
        assert!(!out.contains("{{VERSION}}"));
    }

    #[test]
    fn render_all_kinds_carry_their_config_dir() {
        for (kind, cfg) in [
            ("opencode", "~/.config/opencode"),
            ("pi", "~/.pi"),
            ("grok", "~/.grok"),
        ] {
            let out = render_integration(kind, "0.1.0", "ts").unwrap();
            assert!(out.contains(cfg), "{kind} should mention {cfg}");
            assert!(out.starts_with("---"), "{kind} frontmatter");
        }
    }

    #[test]
    fn render_rejects_unknown_kind() {
        assert!(render_integration("claude", "0.1.0", "ts").is_err());
    }

    #[test]
    fn render_contains_lifecycle_and_context_contract() {
        let out = render_integration("pi", "0.1.0", "ts").unwrap();
        assert!(out.contains("br update bd-<id> --claim"));
        assert!(out.contains("br sync --flush-only"));
        assert!(out.contains("CONTEXT_LIMIT"));
        assert!(out.contains(".flywheel/agent-state/<AGENT_NAME>.json"));
        assert!(out.contains("bv --robot-triage"));
        assert!(out.contains("file_reservation_paths"));
        assert!(out.contains("Never use `bd`"));
    }
}

#[cfg(test)]
mod debate_tests {
    use super::*;

    #[test]
    fn debate_resolves_default_agents() {
        let (_a, _b, j) = resolve_debate_agents("hachiko", None, None, None).unwrap();
        // review-fix F7: defaults expand to the spawned name
        assert_eq!(j, "flywheel-hachiko-gk"); // judge defaults to grok
    }

    #[test]
    fn debate_rejects_same_agent_for_a_and_b() {
        let r = resolve_debate_agents("hachiko", Some("oc"), Some("oc"), None);
        assert!(r.is_err(), "A and B must be distinct");
    }

    #[test]
    fn debate_accepts_explicit_agents() {
        let (a, b, j) =
            resolve_debate_agents("hachiko", Some("pi"), Some("gk"), Some("oc")).unwrap();
        assert_eq!(a, "flywheel-hachiko-pi");
        assert_eq!(b, "flywheel-hachiko-gk");
        assert_eq!(j, "flywheel-hachiko-oc");
    }
}

#[cfg(test)]
mod xproj_tests {
    use super::*;

    #[test]
    fn parse_xproj_request_happy() {
        let t = "[xproj] run 4-worker\nrecipe=backtest-5m\nworkers=4\nproduct=trading-research\nstrategy=CrystalRegime\n";
        let r = parse_xproj_request(t).expect("parse");
        assert_eq!(r.recipe, "backtest-5m");
        assert_eq!(r.workers, 4);
        assert_eq!(r.product, "trading-research");
        assert!(r.params.contains("strategy=CrystalRegime"));
    }

    #[test]
    fn parse_xproj_request_requires_marker_and_recipe() {
        assert!(parse_xproj_request("recipe=foo").is_none());
        assert!(parse_xproj_request("[xproj] hello").is_none());
        assert!(parse_xproj_request("").is_none());
    }

    /// review-fix F3: the send body now carries a `to_project=` routing-context
    /// line; the receiver's parse must tolerate it (recipe/workers still win).
    #[test]
    fn parse_xproj_request_tolerates_to_project_line() {
        let t = "[xproj] run\nrecipe=backtest-5m\nworkers=4\nto_project=/Users/x/workspace/flywheel\nstrategy=X\n";
        let r = parse_xproj_request(t).expect("parse");
        assert_eq!(r.recipe, "backtest-5m");
        assert_eq!(r.workers, 4);
        assert!(r.params.contains("strategy=X"));
    }

    #[test]
    fn parse_xproj_recipe_in_subject_line() {
        let t = "[xproj] run 2-worker recipe=daily-p6\nbody line";
        let r = parse_xproj_request(t).expect("parse");
        assert_eq!(r.recipe, "daily-p6");
    }

    #[test]
    fn extract_xproj_from_inbox_filters() {
        let v = json!([
            {"id": 1, "subject": "hello", "body_md": "nope", "thread_id": "t1", "from": "A"},
            {"id": 2, "subject": "[xproj] run", "body_md": "recipe=daily\nworkers=2", "thread_id": "t2", "from": "B"},
        ]);
        let items = extract_xproj_from_inbox(&v);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "2");
        let mut seen = std::collections::HashSet::new();
        let prompts = xproj_prompts_for_unseen(&items, &seen);
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].1.contains("recipe=daily"));
        seen.insert("2".to_string());
        assert!(xproj_prompts_for_unseen(&items, &seen).is_empty());
    }
}

#[cfg(test)]
mod plan_review_tests {
    use super::*;

    #[test]
    fn parse_review_token_approve_verified() {
        assert_eq!(parse_review_token("APPROVE"), Some(ReviewToken::Approve));
        assert_eq!(
            parse_review_token("approve\nmore text"),
            Some(ReviewToken::Approve)
        );
        assert_eq!(parse_review_token("VERIFIED"), Some(ReviewToken::Verified));
    }

    #[test]
    fn parse_review_token_changes_and_reject() {
        assert_eq!(
            parse_review_token("CHANGES: need more tests"),
            Some(ReviewToken::Changes("need more tests".into()))
        );
        assert_eq!(
            parse_review_token("REJECT: out of scope"),
            Some(ReviewToken::Reject("out of scope".into()))
        );
        assert_eq!(parse_review_token("lgtm maybe"), None);
    }

    #[test]
    fn pri_route_approve_path() {
        let (p, a, rounds) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Approve,
            ".flywheel/plans/x.md",
            0,
            2,
        );
        assert_eq!(p, PriPhase::AwaitingImplement);
        assert_eq!(rounds, 0);
        assert!(matches!(a, PriAction::PromptImplementer { .. }));
        let (p2, a2) = pri_after_implement(".flywheel/plans/x.md");
        assert_eq!(p2, PriPhase::AwaitingVerify);
        assert!(matches!(a2, PriAction::PromptReviewer { .. }));
        let (p3, a3, _) = pri_route(
            PriPhase::AwaitingVerify,
            &ReviewToken::Verified,
            ".flywheel/plans/x.md",
            0,
            2,
        );
        assert_eq!(p3, PriPhase::Done);
        assert_eq!(a3, PriAction::HaltVerified);
    }

    /// review-fix F1: harvest_latest content extraction must prefer the reply
    /// BODY over the subject. The exact agent-mail search envelope (subject +
    /// body_md) must yield the body; subject-only must fall back; empty → None.
    #[test]
    fn extract_latest_body_prefers_body_over_subject() {
        let v = serde_json::json!({
            "result": [{
                "id": 1,
                "subject": "Re: [debate] prop",
                "body_md": "ARGUMENT: buy the dip",
                "from": "TopazOx",
                "thread_id": "debate-1"
            }]
        });
        assert_eq!(
            extract_latest_body(&v),
            Some("ARGUMENT: buy the dip".to_string())
        );
    }

    /// review-fix F1: a body-less envelope falls back to the subject.
    #[test]
    fn extract_latest_body_falls_back_to_subject_when_body_absent() {
        let v = serde_json::json!({
            "result": [{
                "id": 2,
                "subject": "Re: [debate] prop",
                "from": "TopazOx"
            }]
        });
        assert_eq!(
            extract_latest_body(&v),
            Some("Re: [debate] prop".to_string())
        );
    }

    /// review-fix F1: empty result / empty text → None (no phantom "").
    #[test]
    fn extract_latest_body_empty_is_none() {
        assert_eq!(
            extract_latest_body(&serde_json::json!({"result": []})),
            None
        );
        assert_eq!(extract_latest_body(&serde_json::json!({})), None);
        let v = serde_json::json!({
            "result": [{"id": 3, "subject": "   ", "body_md": ""}]
        });
        assert_eq!(extract_latest_body(&v), None);
    }

    /// review-fix F2: the gate is a real state machine, not a single pass.
    /// Walk the driver's phase machine over a scripted reviewer token sequence
    /// (planner always replies PLAN, reviewer replies the given tokens in
    /// order, implementer replies IMPLEMENT_DONE) and assert the visited phase
    /// path + final outcome. This is the test that would have caught the
    /// original never_loop defect: [Changes, Approve] must visit
    /// AwaitingPlanReview TWICE (two CHANGES cycles) before reaching
    /// AwaitingImplement → AwaitingVerify → Done.
    #[test]
    fn plan_review_gate_loops_on_changes_then_verifies() {
        let path = ".flywheel/plans/x.md";
        let mut phases: Vec<PriPhase> = Vec::new();
        let mut phase = PriPhase::AwaitingPlanReview;
        let mut action = pri_after_plan(path).1; // PromptReviewer
        let mut change_rounds = 0u32;
        // scripted reviewer replies: CHANGES then APPROVE (then VERIFIED at verify)
        let mut reviewer_tokens = [
            ReviewToken::Changes("fix a".into()),
            ReviewToken::Approve,
            ReviewToken::Verified,
        ]
        .into_iter();
        let max_rounds = 2u32;
        loop {
            phases.push(phase);
            match phase {
                PriPhase::AwaitingPlanReview => {
                    let token = reviewer_tokens
                        .next()
                        .expect("scripted reviewer ran out of tokens");
                    let (_, a, r) = pri_route(phase, &token, path, change_rounds, max_rounds);
                    action = a;
                    change_rounds = r;
                    // APPROVE routes to implement; CHANGES routes to planner
                    match &action {
                        PriAction::PromptImplementer { .. } => {
                            phase = pri_after_implement(path).0;
                            action = pri_after_implement(path).1;
                        }
                        PriAction::PromptPlanner { .. } => {
                            // planner revises; back to review
                            phase = pri_after_plan(path).0;
                            action = pri_after_plan(path).1;
                        }
                        _ => panic!("unexpected action after review: {action:?}"),
                    }
                }
                PriPhase::AwaitingVerify => {
                    let token = reviewer_tokens
                        .next()
                        .expect("scripted verifier ran out of tokens");
                    let (p, a, _) = pri_route(phase, &token, path, change_rounds, max_rounds);
                    phase = p;
                    action = a;
                }
                PriPhase::Done => break,
                PriPhase::Escalated => panic!("unexpected escalation: {action:?}"),
                other => panic!("unexpected phase: {other:?}"),
            }
        }
        phases.push(PriPhase::Done);
        // visited AwaitingPlanReview twice (initial + after CHANGES re-plan)
        let reviews = phases
            .iter()
            .filter(|p| matches!(p, PriPhase::AwaitingPlanReview))
            .count();
        assert_eq!(reviews, 2, "phase path: {phases:?}");
        assert_eq!(change_rounds, 1);
        assert!(matches!(action, PriAction::HaltVerified));
    }

    /// review-fix F2: change rounds are HONORED — [Changes, Changes, Changes]
    /// with max 2 escalates on the third instead of looping forever.
    #[test]
    fn plan_review_gate_escalates_when_change_rounds_exhausted() {
        let path = ".flywheel/plans/x.md";
        let mut phase = PriPhase::AwaitingPlanReview;
        let mut action = pri_after_plan(path).1;
        let mut change_rounds = 0u32;
        let mut reviewer_tokens = [
            ReviewToken::Changes("a".into()),
            ReviewToken::Changes("b".into()),
            ReviewToken::Changes("c".into()),
        ]
        .into_iter();
        let max_rounds = 2u32;
        let mut iterations = 0u32;
        loop {
            iterations += 1;
            assert!(iterations < 20, "gate must terminate (max_change_rounds)");
            match phase {
                PriPhase::AwaitingPlanReview => {
                    let token = reviewer_tokens
                        .next()
                        .expect("scripted reviewer ran out of tokens");
                    let (p, a, r) = pri_route(phase, &token, path, change_rounds, max_rounds);
                    phase = p;
                    action = a;
                    change_rounds = r;
                    if matches!(action, PriAction::Escalate { .. }) {
                        assert_eq!(change_rounds, 3);
                        assert!(matches!(phase, PriPhase::Escalated));
                        return;
                    }
                    match &action {
                        PriAction::PromptPlanner { .. } => {
                            phase = pri_after_plan(path).0;
                            action = pri_after_plan(path).1;
                        }
                        other => panic!("unexpected: {other:?}"),
                    }
                }
                PriPhase::Escalated => {
                    assert!(matches!(action, PriAction::Escalate { .. }));
                    return;
                }
                other => panic!("unexpected phase: {other:?}"),
            }
        }
    }

    #[test]
    fn pri_route_changes_then_exhaust() {
        let (p, a, r) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Changes("fix".into()),
            "p.md",
            0,
            2,
        );
        assert_eq!(p, PriPhase::AwaitingPlan);
        assert_eq!(r, 1);
        assert!(matches!(a, PriAction::PromptPlanner { .. }));
        // second changes still ok
        let (p2, _, r2) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Changes("more".into()),
            "p.md",
            1,
            2,
        );
        assert_eq!(p2, PriPhase::AwaitingPlan);
        assert_eq!(r2, 2);
        // third exceeds max 2
        let (p3, a3, r3) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Changes("again".into()),
            "p.md",
            2,
            2,
        );
        assert_eq!(p3, PriPhase::Escalated);
        assert_eq!(r3, 3);
        assert!(matches!(a3, PriAction::Escalate { .. }));
    }

    #[test]
    fn pri_route_reject_escalates() {
        let (p, a, _) = pri_route(
            PriPhase::AwaitingPlanReview,
            &ReviewToken::Reject("nope".into()),
            "p.md",
            0,
            2,
        );
        assert_eq!(p, PriPhase::Escalated);
        assert!(matches!(a, PriAction::Escalate { reason } if reason.contains("rejected")));
    }

     #[test]
     fn extract_plan_path_from_reply() {
         assert_eq!(
             extract_plan_path("PLAN: .flywheel/plans/bd-1.md\nthanks", "fallback.md"),
             ".flywheel/plans/bd-1.md"
         );
         assert_eq!(extract_plan_path("no path here", "fb.md"), "fb.md");
     }
}

#[cfg(test)]
mod profile_cli_tests {
    use super::*;
    #[test]
    fn is_builtin_profile_classifies_the_six() {
        for name in ["oracle", "planner", "reviewer", "coder", "scout", "housekeeper"] {
            assert!(is_builtin_profile(name), "{name} is a built-in");
        }
        assert!(!is_builtin_profile("mybot"));
        assert!(!is_builtin_profile(""));
    }

    #[test]
    fn profile_catalog_lists_six_builtins() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        assert_eq!(cat.names().len(), 6);
        for name in ["oracle", "planner", "reviewer", "coder", "scout", "housekeeper"] {
            let p = cat.get(name).expect(name);
            // every built-in carries the Spec 1b preamble (not a stub)
            assert!(p.behavior.len() > 200, "{name} preamble missing");
        }
    }

    #[test]
    fn profile_run_resolves_host_scope_and_spec() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        let r = cat
            .resolve("housekeeper", None, "npub-op", &|_, _| None)
            .expect("resolve");
        assert_eq!(r.spec.harness, "pi");
        assert_eq!(r.spec.model, "claude-sonnet-4");
        assert_eq!(r.spec.backend, "herdr");
        assert_eq!(r.signing, profile::SigningContext::Operator("npub-op".into()));
    }

    #[test]
    fn squad_show_resolves_recipe_alias_to_members() {
        // A legacy recipe name resolves as a squad (alias) — the P6.2 core.
        // We exercise the mapping logic directly (Recipe -> Squad) since the
        // CLI fn needs a live config dir + backend.
        let r = config::Recipe {
            oc: Some(2),
            pi: Some(1),
            gk: None,
            extra: Default::default(),
        };
        let squad = profile::Squad::from_recipe(&r);
        let members = squad.members();
        assert_eq!(members, vec![("oc", 2), ("pi", 1)]);
    }

    #[test]
    fn squad_list_lists_builtin_profiles_as_singles() {
        // With no recipes.toml, `squad list` falls back to 1-member squads for
        // every built-in profile (zero-config squad surface).
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        let recipes = config::Recipes::default().recipes;
        let names: Vec<String> = cat.names();
        for name in &names {
            assert!(
                !recipes.contains_key(name),
                "no user recipe named {name} in a fresh install"
            );
        }
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn snapshot_runs_returns_well_formed_entries() {
        // Runs section is always a JSON array; every entry carries the fields
        // the agent driver contracts on (id/project/kind/owner/state/ts).
        let runs = snapshot_runs();
        for r in &runs {
            assert!(r.get("id").and_then(Value::as_str).is_some(), "id");
            assert!(r.get("project").and_then(Value::as_str).is_some(), "project");
            assert!(r.get("kind").and_then(Value::as_str).is_some(), "kind");
            assert!(r.get("owner").and_then(Value::as_str).is_some(), "owner");
            assert!(r.get("state").and_then(Value::as_str).is_some(), "state");
            assert!(r.get("ts").and_then(Value::as_str).is_some(), "ts");
        }
    }
}

#[cfg(test)]
mod workflow_cli_lint_tests {
    use super::*;
    use toron_workflow::schema::parse_workflow;

    /// The flywheel-tier lint fails closed on an unresolved `@profile`
    /// (the bead's fail-stop contract).
    #[test]
    fn lint_rejects_unresolved_profile() {
        let wf = parse_workflow(
            r##"
name: t
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: a
    action: dispatch_job
    assignee: x
    spec: { profile: "@doesnotexist" }
"##,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf);
        assert!(blocking > 0, "unresolved profile must be a blocking issue");
    }

    /// An unresolved `stage:` alias fails closed.
    #[test]
    fn lint_rejects_unresolved_stage() {
        let wf = parse_workflow(
            r##"
name: t
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: a
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "not-a-real-stage" }
"##,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf);
        assert!(blocking > 0, "unresolved stage must be a blocking issue");
    }

    /// A `{{inputs.x}}` ref with no matching `inputs:` declaration fails closed.
    #[test]
    fn lint_rejects_undeclared_input_ref() {
        let wf = parse_workflow(
            r##"
name: t
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: a
    action: send_dm
    to: x
    body: "{{inputs.ghost}}"
"##,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf);
        assert!(
            blocking > 0,
            "undeclared input ref must be a blocking issue"
        );
    }

    /// A nested ref into a declared OBJECT input (`{{inputs.bundle.run_id}}`)
    /// resolves against the declared root and lints clean — the engine drills
    /// into the object at runtime (bd-p4-migrate-sagas-7rmo design needs it).
    #[test]
    fn lint_accepts_nested_ref_into_declared_object_input() {
        let wf = parse_workflow(
            r##"
name: t
trigger:
  type: webhook
inputs:
  - name: bundle
steps:
  - name: a
    action: send_dm
    to: x
    body: "{{inputs.bundle.run_id}} {{inputs.bundle.workflow}}"
"##,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf);
        assert_eq!(blocking, 0, "nested ref into declared root must lint clean");
    }

    /// A clean workflow passes the flywheel tier (no blocking issues).
    #[test]
    fn lint_clean_workflow_passes() {
        let wf = parse_workflow(
            r##"
name: t
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: a
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "systematic-execute" }
"##,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf);
        assert_eq!(blocking, 0, "clean workflow must lint clean");
    }

    /// The CI gate (bead contract: lint over EVERY built-in on every change):
    /// every compiled-in workflow passes the FULL gate — engine tier
    /// (lint_workflow) + flywheel tier (profile/stage/inputs resolution).
    #[test]
    fn every_builtin_passes_full_lint_gate() {
        for (name, wf) in builtin_workflows::builtin_workflows() {
            let engine = toron_workflow::lint_workflow(&wf);
            assert!(
                engine.is_clean(),
                "built-in {name} fails engine lint: {:?}",
                engine.issues
            );
            let tier = lint_flywheel_tier(&name, &wf);
            assert_eq!(
                tier, 0,
                "built-in {name} fails flywheel-tier lint (profile/stage/inputs)"
            );
        }
    }
}

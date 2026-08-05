//! flywheel — multi-agent swarm orchestrator on the herdr backend.
//!
//! NTM-parity daily loop for opencode/pi/grok: spawn swarms, dispatch work,
//! monitor states, checkpoint progress. All tracker state lives in br/bv;
//! all coordination lives in agent-mail; herdr is the backend.

mod agentstate;
mod backend;
mod config;
mod durable;
mod herdr;
mod mail;
mod output;
mod state;
mod work;

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use config::{Config, Recipe, WorkflowStep};
use herdr::{
    classify_send_result, AgentInfo, AgentStatus, Client, SendVerdict, Subscription, WaitOptions,
};
use output::{die, dim, error, kind_paint, ok as paint_ok, print_json, status_paint};
use state::{log_event, EventKind};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---- stage prompts (verbatim from the author's daily-driver set) ----

const STAGES: &[(&str, &str)] = &[
    ("p1", "I want you to sort of randomly explore the code files in this project, choosing code files to deeply investigate and understand and trace their functionality and execution flows through the related code files which they import or which they are imported by. Once you understand the purpose of the code in the larger context of the workflows, I want you to do a super careful, methodical, and critical check with \"fresh eyes\" to find any obvious bugs, problems, errors, issues, silly mistakes, etc. and then systematically and meticulously and intelligently correct them. Be sure to comply with ALL rules in AGENTS dot md."),
    ("p2", "Ok can you now turn your attention to reviewing the code written by your fellow agents and checking for any issues, bugs, errors, problems, inefficiencies, security problems, reliability issues, etc. and carefully diagnose their underlying root causes using first-principle analysis and then fix or revise them if necessary? Don't restrict yourself to the latest commits, cast a wider net and go super deep! Use ultrathink."),
    ("p4", "OK so please take ALL of that and elaborate on it more and then create a comprehensive and granular set of beads for all this with tasks, subtasks, and dependency structure overlaid, with detailed comments so that the whole thing is totally self-contained and self-documenting (including relevant background, reasoning/justification, considerations, etc.-- anything we'd want our \"future self\" to know about the goals and intentions and thought process and how it serves the over-arching goals of the project.) Follow the beads-workflow discipline: check your beads N times before implementing, keep each bead self-contained (background, reasoning, acceptance criteria), and link dependencies with `br dep add`."),
    ("p5", "Check over each bead super carefully-- are you sure it makes sense? Is it optimal? Could we change anything to make the system work better for users? If so, revise the beads. It's a lot easier and faster to operate in \"plan space\" before we start implementing these things!"),
    ("p6", "OK, so start systematically and methodically and meticulously and diligently executing those remaining beads tasks that you created in the optimal logical order! For each bead follow the FULL lifecycle: triage (`bv --robot-triage` / `br ready --json`) -> claim (`br update --claim --actor \"$AGENT_NAME\" --json`; never work unclaimed) -> reserve files via agent-mail -> announce -> execute one narrow slice -> quality gates -> verify acceptance criteria -> close with an honest reason (`br close`) -> `br sync --flush-only` + commit `.beads/` -> handoff message. Prefer delegating to the `beads` subagent when available."),
    ("p8", "Now, based on your knowledge of the project, commit all changed files now in a series of logically connected groupings with super detailed commit messages for each and then push. Take your time to do it right. Don't edit the code at all. Don't commit obviously ephemeral files. Use ultrathink."),
];

fn stage_prompt(name: &str) -> Result<&'static str> {
    STAGES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, p)| *p)
        .ok_or_else(|| {
            anyhow!(
                "unknown stage: {name} (known: {})",
                STAGES
                    .iter()
                    .map(|(n, _)| *n)
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
        message_id: u64,
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
    /// fetch inbox for the flywheel identity
    Inbox {
        project: String,
        #[arg(long)]
        urgent: bool,
        #[arg(long, default_value = "20")]
        limit: u32,
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
    },
    /// release this identity's reservations
    Unlock { project: String },
    /// check reservation conflicts for paths
    Conflicts {
        project: String,
        #[arg(long)]
        paths: Vec<String>,
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
    let matches: Vec<herdr::WorkspaceInfo> = c
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
        return Ok(herdr::resolve_workspace_id(&matches, &agents, &prefix));
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
        to.split(',')
            .map(|s| agent_name(project, s.trim()))
            .collect()
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
    backend: &mut dyn backend::MultiplexerBackend,
    project: &str,
) -> Result<String> {
    let label = swarm_label(project);
    let matches: Vec<herdr::WorkspaceInfo> = backend
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
    backend: &mut dyn backend::MultiplexerBackend,
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
        to.split(',')
            .map(|s| agent_name(project, s.trim()))
            .collect()
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
            "backend": format!("herdr (socket protocol v{})", herdr::HDR_PROTOCOL),
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
            *message_id,
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
        }) => cmd_internal_respawn(&cfg, project, name, kind),
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
    Ok(json!({
        "tool": "flywheel",
        "version": VERSION,
        "ts": state::now_iso(),
        "workspaces": fly,
        "agents": agents.iter().map(|a| json!({
            "name": a.name, "kind": a.agent, "status": a.agent_status.as_str(),
            "workspace_id": a.workspace_id, "pane_id": a.pane_id
        })).collect::<Vec<_>>()
    }))
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
        if let Some(n) = n {
            if n > 0 {
                out.push((k.into(), n));
            }
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
    let (ws, root) = c.workspace_create(Some(dir.to_str().unwrap()), Some(&label), !no_focus)?;
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
                let p = c.pane_split(&prev, "down", Some(dir.to_str().unwrap()))?;
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
        if let Err(e) = c.agent_start(name, kind, pane_id, 45_000, args) {
            if attempt == 0 {
                eprintln!("agent.start error: {e:#}");
            }
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
                if a.name.as_deref() != Some(name) {
                    if let Err(e) = c.agent_rename(&a.pane_id, name) {
                        eprintln!("agent.rename error: {e:#}");
                    }
                }
                if let Ok(agents) = c.agent_list() {
                    if let Some(a) = agents.iter().find(|a| a.pane_id == pane_id) {
                        return Ok(a.clone());
                    }
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
    let mut mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(_) => return, // mail not configured — silent skip
    };
    if let Err(e) = mail.ensure_project(&key) {
        eprintln!("flywheel: mail ensure_project failed for {name}: {e}");
        return;
    }
    // bd-odc fix: agent-mail's register_agent REJECTS non-adjective+noun
    // names, and flywheel spawns `flywheel-<base>-<kind>` names. Map to a
    // stable adjective+noun (agent_mail_name), keep the herdr name in
    // task_description so list_agents is the reverse lookup.
    let mail_name = agent_mail_name(name);
    let task = format!("{name} (flywheel agent for {project})");
    if let Err(e) = mail.register_agent(&key, &mail_name, kind, kind, &task) {
        // Best-effort: a registration failure must never fail the spawn. The
        // pane-identity bridge below still works (the identity file is the
        // pane→name source the server reads, registration or not).
        eprintln!("flywheel: mail register_agent failed for {name}: {e}");
    }
    // bridge the identity to the herdr pane (best-effort, like register).
    // The file holds the REGISTERED identity (the mapped adjective+noun) so
    // resolve_pane_identity returns a name that exists in the registry — the
    // herdr name is not registrable and would resolve to a dead end.
    if let Err(e) = mail.write_pane_identity(&key, pane_id, &mail_name) {
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
        if let Some(rest) = lower.strip_prefix(adj) {
            if !rest.is_empty() && nouns.contains(&rest) {
                let mut adj = (*adj).to_owned();
                adj.get_mut(0..1)?.make_ascii_uppercase();
                let mut noun = rest.to_owned();
                noun.get_mut(0..1)?.make_ascii_uppercase();
                return Some(format!("{adj}{noun}"));
            }
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
            let p = c.pane_split(&prev, "down", Some(dir.to_str().unwrap()))?;
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
                    let st = AgentStatus::parse(st)
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
            let st = AgentStatus::parse(st)
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
        (Some(s), _) => stage_prompt(s)?.to_string(),
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
                status_paint(AgentStatus::parse(status).unwrap_or(AgentStatus::Unknown))
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
    let until_status: Option<AgentStatus> = until.map(AgentStatus::parse).transpose()?;
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
        if let Some(u) = until_status {
            if st == u && all_matching(&mut c, &watch_names, u)? {
                return Ok(());
            }
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
    let mut stream = match c.subscribe(&subs) {
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
                        herdr::HerdrEventData::PaneAgentStatusChanged(change)
                        | herdr::HerdrEventData::PaneAgentStatusChangedSub(change) => {
                            let Some(name) = watch_name_for_pane(&watch_targets, &change.pane_id)
                            else {
                                continue;
                            };
                            if std::io::stdout().is_terminal() {
                                println!("{} {}", dim(name), status_paint(change.agent_status));
                            }
                            if let Some(u) = until_status {
                                if change.agent_status == u
                                    && all_matching(&mut c, &watch_names, u)?
                                {
                                    return Ok(());
                                }
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
        if watch_names.contains(&name) {
            if let Some(u) = until_status {
                if a.agent_status == u && all_matching(c, watch_names, u)? {
                    return Ok(true);
                }
            }
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
        c.pane_zoom(&a.pane_id, mode)?;
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
    if let Ok(mut mail) = backend::mail_backend(cfg) {
        if let Some(key) = project_dir(cfg, project).to_str() {
            for a in &agents {
                if let Err(e) = mail.clear_pane_identity(key, &a.pane_id) {
                    eprintln!(
                        "flywheel: mail clear_pane_identity failed for {} on {}: {e}",
                        a.name.as_deref().unwrap_or("<unnamed>"),
                        a.pane_id
                    );
                }
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
    let Some(mail) = mail::Mail::from_config(cfg.agent_mail.as_ref())? else {
        bail!(
            "agent-mail not configured — set [agent_mail] url/token/name in {}",
            config::config_file().display()
        );
    };
    let mut mail = mail;
    let dir = project_dir(cfg, project);
    let key = dir.to_str().unwrap();
    mail.ensure_project(key)?;
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
        let agents = mail.list_agents(key)?;
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
    mail.send(key, &targets, &subject, &body, "high", true, None)?;
    println!("assigned {} to {}", bead_ids.join(", "), targets.join(", "));
    Ok(())
}

// ---- mail ----

fn mail_or_die(cfg: &Config) -> Result<mail::Mail> {
    mail::Mail::from_config(cfg.agent_mail.as_ref())?.ok_or_else(|| {
        anyhow!(
            "agent-mail not configured — set [agent_mail] url/token/name in {}",
            config::config_file().display()
        )
    })
}

/// Parsed cross-project swarm request (subject `[xproj] …` + body fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XprojRequest {
    pub recipe: String,
    pub workers: u32,
    pub params: String,
    pub product: String,
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
            let mut mail = mail_or_die(cfg)?;
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            let to_key = project_dir(cfg, to_project).to_str().unwrap().to_string();
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
            let mut mail = mail_or_die(cfg)?;
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            mail.ensure_project(&key)?;
            let to_mail = resolve_mail_target(to);
            let ts = crate::state::now_iso().replace(['-', ':'], "");
            let thread = format!("xproj-{recipe}-{ts}");
            let subject = format!("[xproj] run {workers}-worker swarm recipe={recipe}");
            let body_md = format!(
                "recipe={recipe}\nworkers={workers}\nproduct={product}\n\n{body}\n\n\
                 Reply results in this thread. Receiver: spawn workers in your project, \
                 run the recipe/workflow, reply a summary here."
            );
            // send into the local project inbox of the remote agent identity
            // (cross-project delivery relies on approved contact + product search)
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
                "send": v,
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
    let mut mail = mail_or_die(cfg)?;
    match cmd {
        MailCmd::Inbox {
            project,
            urgent,
            limit,
        } => {
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
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
        } => {
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            mail.ensure_project(&key)?;
            // herdr names resolve to their registered identities (bd-odc)
            let targets: Vec<String> = to
                .split(',')
                .map(|s| resolve_mail_target(s.trim()))
                .collect();
            let v = mail.send(&key, &targets, subject, body, importance, *ack, None)?;
            print_json(&v);
        }
        MailCmd::Locks {
            project,
            paths,
            ttl_seconds,
            exclusive,
            reason,
        } => {
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            mail.ensure_project(&key)?;
            let v = mail.reserve(&key, paths, *ttl_seconds, *exclusive, reason)?;
            print_json(&v);
        }
        MailCmd::Unlock { project } => {
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            let v = mail.release(&key)?;
            print_json(&v);
        }
        MailCmd::Conflicts { project, paths } => {
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            mail.ensure_project(&key)?;
            let v = mail.conflicts(&key, paths)?;
            print_json(&v);
        }
        MailCmd::Agents { project } => {
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            mail.ensure_project(&key)?;
            let v = mail.list_agents(&key)?;
            print_json(&v);
        }
        MailCmd::Contact { cmd } => match cmd {
            MailContactCmd::Request {
                project,
                to,
                to_project,
                reason,
            } => {
                let key = project_dir(cfg, project).to_str().unwrap().to_string();
                mail.ensure_project(&key)?;
                let from = mail.name.clone();
                let to_mail = resolve_mail_target(to);
                let to_proj = to_project
                    .as_ref()
                    .map(|p| project_dir(cfg, p).to_str().unwrap().to_string());
                let v = mail.request_contact(
                    &key,
                    &from,
                    &to_mail,
                    to_proj.as_deref(),
                    reason,
                    None,
                )?;
                print_json(&v);
            }
            MailContactCmd::Approve { project, from } => {
                let key = project_dir(cfg, project).to_str().unwrap().to_string();
                let to = mail.name.clone();
                let from_mail = resolve_mail_target(from);
                let v = mail.respond_contact(&key, &to, &from_mail, true)?;
                print_json(&v);
            }
            MailContactCmd::Deny { project, from } => {
                let key = project_dir(cfg, project).to_str().unwrap().to_string();
                let to = mail.name.clone();
                let from_mail = resolve_mail_target(from);
                let v = mail.respond_contact(&key, &to, &from_mail, false)?;
                print_json(&v);
            }
            MailContactCmd::List { project } => {
                let key = project_dir(cfg, project).to_str().unwrap().to_string();
                let name = mail.name.clone();
                let v = mail.list_contacts(&key, &name)?;
                print_json(&v);
            }
            MailContactCmd::Policy { project, policy } => {
                let key = project_dir(cfg, project).to_str().unwrap().to_string();
                let name = mail.name.clone();
                let v = mail.set_contact_policy(&key, &name, policy)?;
                print_json(&v);
            }
        },
        MailCmd::Product { cmd } => match cmd {
            MailProductCmd::Ensure { name } => {
                let v = mail.ensure_product(name)?;
                print_json(&v);
            }
            MailProductCmd::Link { product, project } => {
                let key = project_dir(cfg, project).to_str().unwrap().to_string();
                mail.ensure_project(&key)?;
                let v = mail.products_link(product, &key)?;
                print_json(&v);
            }
            MailProductCmd::Search {
                product,
                query,
                limit,
            } => {
                let v = mail.search_product(product, query, *limit)?;
                print_json(&v);
            }
            MailProductCmd::Inbox { product, limit } => {
                let name = mail.name.clone();
                let v = mail.inbox_product(product, &name, *limit)?;
                print_json(&v);
            }
        },
    }
    Ok(())
}

// ---- worktree ----

fn cmd_worktree(cfg: &Config, cmd: &WorktreeCmd) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    match cmd {
        WorktreeCmd::List { project } => {
            let ws_id = find_workspace(&mut c, project)?;
            let v = c.worktree_list(Some(&ws_id))?;
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
                Some(project_dir(cfg, project).to_str().unwrap()),
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
                status_paint(AgentStatus::parse(now_st).unwrap_or(AgentStatus::Unknown))
            );
        }
    }
    let head = state::git_head(project_dir(cfg, project).to_str().unwrap());
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
                    (Some(s), _) => stage_prompt(s)?.to_string(),
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
                let st = AgentStatus::parse(until)?;
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
                        (Some(s), _) => stage_prompt(s)?.to_string(),
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
                items.push(p.to_string());
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
        if let Some(d) = deadline {
            if std::time::Instant::now() >= d {
                bail!(
                    "loop timed out after {}s (queue remaining: {})",
                    timeout_ms.unwrap_or(0) / 1000,
                    queue_items.len()
                );
            }
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
                            if let Ok(mut m) = mail_or_die(cfg) {
                                let key = project_dir(cfg, project).to_str().unwrap().to_string();
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
                                eprintln!("flywheel: --on-blocked mail needs [agent_mail] config");
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
        .and_then(|o| herdr::parse_protocol(&String::from_utf8_lossy(&o.stdout)))
    {
        Some(p) => match protocol_check(p, herdr::HDR_PROTOCOL) {
            ProtocolCheck::Match => println!(
                "herdr protocol: {} (verified {})",
                paint_ok(&p.to_string()),
                herdr::HDR_PROTOCOL
            ),
            ProtocolCheck::Newer => {
                println!(
                    "herdr protocol: {} (verified {})",
                    crate::output::warn(&p.to_string()),
                    herdr::HDR_PROTOCOL
                );
                eprintln!(
                    "flywheel: INFO herdr protocol {p} > verified {} — forward-compatible (client ignores what it doesn't know), but flywheel may not use new features; rebuild against the newer schema when ready",
                    herdr::HDR_PROTOCOL
                );
            }
            ProtocolCheck::Older => {
                ok = false;
                println!(
                    "herdr protocol: {} (verified {})",
                    crate::output::error(&p.to_string()),
                    herdr::HDR_PROTOCOL
                );
                eprintln!(
                    "flywheel: ERROR herdr protocol {p} < verified {} — server too old, our requests may be rejected; upgrade herdr or rebuild flywheel against the pinned schema",
                    herdr::HDR_PROTOCOL
                );
            }
        },
        None => {
            println!(
                "herdr protocol: {} (verified {})",
                crate::output::warn("unknown"),
                herdr::HDR_PROTOCOL
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

    match mail::Mail::from_config(cfg.agent_mail.as_ref()) {
        Ok(Some(mut m)) => match m.health() {
            Ok(_) => println!("agent-mail: {}", status_paint(AgentStatus::Idle)),
            Err(e) => {
                ok = false;
                println!("agent-mail: {} ({e:#})", error("FAIL"));
            }
        },
        Ok(None) => println!("agent-mail: {}", dim("not configured (set [agent_mail])")),
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
            // mail
            for (name, stub) in [("agentmail", false), ("toron", true)] {
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
            // health: multiplexer = socket reachable; mail = agentmail /health
            let mux_health = backend::multiplexer_backend(cfg)
                .and_then(|mut b| b.agent_list().map(|_| true))
                .unwrap_or(false);
            let mail_health = if mail == "agentmail" {
                backend::mail_backend(cfg)
                    .and_then(|mut m| m.health().map(|_| true))
                    .unwrap_or(false)
            } else {
                false // stub backends aren't healthy by construction
            };
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
    let ts = crate::state::now_iso().replacen('-', "", 10); // compact ts for the thread id
    let thread = format!("debate-{ts}");

    // agent names: explicit or default to the swarm's first oc/pi/gk
    let (a, b, j) = resolve_debate_agents(cfg, proposition, agent_a, agent_b, judge, &thread)?;

    let mut mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => bail!("mail backend unavailable for debate: {e:#}"),
    };
    let mut mux = backend::multiplexer_backend(cfg)?;
    // scope the debate to the project dir (the proposition may be a project
    // name; default to the flywheel project for ad-hoc debates)
    let key = if cfg.workspace_root().join(proposition).exists() {
        project_dir(cfg, proposition).to_str().unwrap().to_string()
    } else {
        project_dir(cfg, "flywheel").to_str().unwrap().to_string()
    };
    mail.ensure_project(&key)?;

    let mail_target = |agent: &str| resolve_mail_target(agent).to_string();

    // opening: A argues FOR
    let opening = format!(
        "You are debating proposition: {proposition}\n\nArgue FOR it. Be concise and concrete (<= 120 words). Provide your single strongest argument with a brief reason.\n\nAfter replying in this thread, acknowledge the orchestrator's opening message to signal your turn is done."
    );
    mail.send(
        &key,
        &[mail_target(a)],
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
        let speaker = if round == 1 {
            a
        } else if round % 2 == 1 {
            a
        } else {
            b
        };
        let opponent = if speaker == a { b } else { a };
        let _ = opponent;
        if round > 1 {
            let reply = last_reply.clone().unwrap_or_default();
            let counter = format!(
                "You are debating proposition: {proposition}\n\nCounter the opponent's latest argument with a NEW point (do not repeat). <= 120 words. Acknowledge when done.\n\nOpponent's latest argument: {reply}"
            );
            mail.send(
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
        settle_agent(mux.as_mut(), speaker)?;
        // harvest the speaker's latest message in the thread
        last_reply = harvest_latest(mail.as_mut(), &key, &thread, &resolve_mail_target(speaker))?;
        println!("debate {thread}: {} replied (round {})", speaker, round);
    }

    // judge: read the thread, pick a winner
    let judge_prompt = format!(
        "You are the judge of this debate on: {proposition}\n\nRead the full thread via summarize_thread. Pick a winner in <= 100 words: name the winner's argument and why it is stronger. Reply in thread and acknowledge when done."
    );
    mail.send(
        &key,
        &[mail_target(j)],
        &format!("[debate] {proposition} — judge"),
        &judge_prompt,
        "high",
        true,
        Some(&thread),
    )?;
    settle_agent(mux.as_mut(), j)?;
    let verdict = harvest_latest(mail.as_mut(), &key, &thread, &resolve_mail_target(j))?;
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
    let planner = planner.unwrap_or("oc");
    let reviewer = reviewer.unwrap_or("pi");
    let implementer = implementer.unwrap_or("gk");
    if planner == reviewer || planner == implementer || reviewer == implementer {
        bail!("plan-review needs three distinct agents (planner/reviewer/implementer)");
    }

    let plan_path = match bead {
        Some(id) => format!(".flywheel/plans/{id}.md"),
        None => format!(".flywheel/plans/plan-{ts}.md"),
    };

    let mut mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => bail!("mail backend unavailable for plan-review: {e:#}"),
    };
    let mut mux = backend::multiplexer_backend(cfg)?;
    let key = project_dir(cfg, project).to_str().unwrap().to_string();
    mail.ensure_project(&key)?;

    let mail_target = |agent: &str| resolve_mail_target(agent);
    let subject = format!("[plan-review] {project}");

    eprintln!(
        "plan-review {thread}: planner={} reviewer={} implementer={} max_change_rounds={max_rounds} paper-first",
        planner, reviewer, implementer
    );

    // --- plan ---
    let mut phase = PriPhase::AwaitingPlan;
    let mut change_rounds = 0u32;
    let mut current_path = plan_path.clone();

    let open = pri_planner_prompt(feature, &current_path);
    mail.send(
        &key,
        &[mail_target(planner)],
        &subject,
        &open,
        "high",
        true,
        Some(&thread),
    )?;
    settle_agent(mux.as_mut(), planner)?;
    let planner_mail = mail_target(planner);
    let plan_reply =
        harvest_latest(mail.as_mut(), &key, &thread, &planner_mail)?.unwrap_or_default();
    current_path = extract_plan_path(&plan_reply, &current_path);
    let (next_phase, action) = pri_after_plan(&current_path);
    phase = next_phase;

    // --- gate loop ---
    loop {
        match &action {
            PriAction::PromptReviewer { msg } => {
                mail.send(
                    &key,
                    &[mail_target(reviewer)],
                    &subject,
                    msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(mux.as_mut(), reviewer)?;
                let rev_mail = mail_target(reviewer);
                let body =
                    harvest_latest(mail.as_mut(), &key, &thread, &rev_mail)?.unwrap_or_default();
                let token = parse_review_token(&body).ok_or_else(|| {
                    anyhow!(
                        "reviewer reply missing APPROVE|CHANGES:|VERIFIED|REJECT: token: {body}"
                    )
                })?;
                let (p, a, rounds) =
                    pri_route(phase, &token, &current_path, change_rounds, max_rounds);
                phase = p;
                change_rounds = rounds;
                match a {
                    PriAction::PromptImplementer { msg } => {
                        // build-slot instruction is in the prompt; agent self-gates
                        mail.send(
                            &key,
                            &[mail_target(implementer)],
                            &subject,
                            &msg,
                            "high",
                            true,
                            Some(&thread),
                        )?;
                        settle_agent(mux.as_mut(), implementer)?;
                        let (p2, a2) = pri_after_implement(&current_path);
                        phase = p2;
                        // continue with verify prompt
                        mail.send(
                            &key,
                            &[mail_target(reviewer)],
                            &subject,
                            match &a2 {
                                PriAction::PromptReviewer { msg } => msg.as_str(),
                                _ => unreachable!(),
                            },
                            "high",
                            true,
                            Some(&thread),
                        )?;
                        settle_agent(mux.as_mut(), reviewer)?;
                        let rev2 =
                            harvest_latest(mail.as_mut(), &key, &thread, &mail_target(reviewer))?
                                .unwrap_or_default();
                        let token2 = parse_review_token(&rev2).ok_or_else(|| {
                            anyhow!("verify reply missing VERIFIED|REJECT: token: {rev2}")
                        })?;
                        let (p3, a3, _) =
                            pri_route(phase, &token2, &current_path, change_rounds, max_rounds);
                        phase = p3;
                        match a3 {
                            PriAction::HaltVerified => {
                                println!(
                                    "plan-review {thread}: VERIFIED (paper-first — human merge/close)"
                                );
                                return Ok(());
                            }
                            PriAction::Escalate { reason } => {
                                eprintln!("plan-review {thread}: escalate: {reason}");
                                return Ok(());
                            }
                            other => bail!("unexpected after verify: {other:?}"),
                        }
                    }
                    PriAction::PromptPlanner { msg } => {
                        mail.send(
                            &key,
                            &[mail_target(planner)],
                            &subject,
                            &msg,
                            "high",
                            true,
                            Some(&thread),
                        )?;
                        settle_agent(mux.as_mut(), planner)?;
                        let pr =
                            harvest_latest(mail.as_mut(), &key, &thread, &mail_target(planner))?
                                .unwrap_or_default();
                        current_path = extract_plan_path(&pr, &current_path);
                        let (p2, a2) = pri_after_plan(&current_path);
                        phase = p2;
                        // re-enter by sending review prompt (loop)
                        let msg = match a2 {
                            PriAction::PromptReviewer { msg } => msg,
                            _ => unreachable!(),
                        };
                        // recurse one review by continuing loop with synthetic action
                        // simplest: re-send and continue parsing at top via goto-style
                        mail.send(
                            &key,
                            &[mail_target(reviewer)],
                            &subject,
                            &msg,
                            "high",
                            true,
                            Some(&thread),
                        )?;
                        // fall through by continuing outer loop — but we need
                        // the next harvest. Inline one more review cycle:
                        settle_agent(mux.as_mut(), reviewer)?;
                        let body =
                            harvest_latest(mail.as_mut(), &key, &thread, &mail_target(reviewer))?
                                .unwrap_or_default();
                        let token = parse_review_token(&body).ok_or_else(|| {
                            anyhow!("reviewer reply missing token after CHANGES: {body}")
                        })?;
                        let (p, a, rounds) =
                            pri_route(phase, &token, &current_path, change_rounds, max_rounds);
                        phase = p;
                        change_rounds = rounds;
                        match a {
                            PriAction::PromptImplementer { msg } => {
                                mail.send(
                                    &key,
                                    &[mail_target(implementer)],
                                    &subject,
                                    &msg,
                                    "high",
                                    true,
                                    Some(&thread),
                                )?;
                                settle_agent(mux.as_mut(), implementer)?;
                                let (_, a2) = pri_after_implement(&current_path);
                                mail.send(
                                    &key,
                                    &[mail_target(reviewer)],
                                    &subject,
                                    match &a2 {
                                        PriAction::PromptReviewer { msg } => msg.as_str(),
                                        _ => unreachable!(),
                                    },
                                    "high",
                                    true,
                                    Some(&thread),
                                )?;
                                settle_agent(mux.as_mut(), reviewer)?;
                                let rev2 = harvest_latest(
                                    mail.as_mut(),
                                    &key,
                                    &thread,
                                    &mail_target(reviewer),
                                )?
                                .unwrap_or_default();
                                let token2 = parse_review_token(&rev2).ok_or_else(|| {
                                    anyhow!("verify reply missing token: {rev2}")
                                })?;
                                let (p3, a3, _) = pri_route(
                                    PriPhase::AwaitingVerify,
                                    &token2,
                                    &current_path,
                                    change_rounds,
                                    max_rounds,
                                );
                                phase = p3;
                                match a3 {
                                    PriAction::HaltVerified => {
                                        println!(
                                            "plan-review {thread}: VERIFIED (paper-first)"
                                        );
                                        return Ok(());
                                    }
                                    PriAction::Escalate { reason } => {
                                        eprintln!("plan-review {thread}: escalate: {reason}");
                                        return Ok(());
                                    }
                                    other => bail!("unexpected after verify: {other:?}"),
                                }
                            }
                            PriAction::Escalate { reason } => {
                                eprintln!("plan-review {thread}: escalate: {reason}");
                                return Ok(());
                            }
                            other => bail!(
                                "after second review only APPROVE/REJECT/CHANGES expected, got {other:?}"
                            ),
                        }
                    }
                    PriAction::Escalate { reason } => {
                        eprintln!("plan-review {thread}: escalate: {reason}");
                        return Ok(());
                    }
                    other => bail!("unexpected route after plan review: {other:?}"),
                }
            }
            PriAction::HaltVerified => {
                println!("plan-review {thread}: VERIFIED (paper-first — human merge/close)");
                return Ok(());
            }
            PriAction::Escalate { reason } => {
                eprintln!("plan-review {thread}: escalate: {reason}");
                return Ok(());
            }
            other => bail!("unexpected action in gate loop: {other:?}"),
        }
    }
}

/// Resolve the A/B/judge agents for a debate: explicit names, or default to
/// the swarm's first agents. Returns the (a, b, j) herdr agent names.
fn resolve_debate_agents<'a>(
    _cfg: &Config,
    _proposition: &str,
    agent_a: Option<&'a str>,
    agent_b: Option<&'a str>,
    judge: Option<&'a str>,
    _thread: &str,
) -> Result<(&'a str, &'a str, &'a str)> {
    let a = agent_a.unwrap_or("oc");
    let b = agent_b.unwrap_or("pi");
    let j = judge.unwrap_or("gk");
    if a == b {
        bail!("debate needs two distinct agents (A and B are both '{a}')");
    }
    Ok((a, b, j))
}

/// Wait for an agent to settle (done/idle/blocked) — the turn-finality gate.
fn settle_agent(mux: &mut dyn backend::MultiplexerBackend, agent: &str) -> Result<()> {
    mux.agent_wait(
        agent,
        &[
            crate::herdr::AgentStatus::Done,
            crate::herdr::AgentStatus::Idle,
            crate::herdr::AgentStatus::Blocked,
        ],
        Some(1_800_000),
    )
    .map_err(|e| anyhow!("agent {agent} settle failed: {e}"))?;
    Ok(())
}

/// Harvest the latest message in a thread from a sender (recency-ranked).
fn harvest_latest(
    mail: &mut dyn backend::MailBackend,
    key: &str,
    thread: &str,
    sender: &str,
) -> Result<Option<String>> {
    let v = mail.search(
        key,
        "",
        1,
        &backend::SearchFilter {
            thread_id: Some(thread.to_string()),
            sender: Some(sender.to_string()),
            ranking: Some("recency".to_string()),
            ..Default::default()
        },
    )?;
    let text = v
        .get("result")
        .and_then(serde_json::Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|m| m.get("subject").and_then(serde_json::Value::as_str))
        .map(|s| s.to_string());
    Ok(text)
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
    let mut backend = backend::multiplexer_backend(cfg)?;
    let ws_id = match find_workspace_dyn(backend.as_mut(), project) {
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

/// Map a typed push event to a status item, advancing `seq`. Returns `None`
/// for non-status events (output_changed etc. — filtered at subscribe time
/// but defensive).
fn summarize_status_event(ev: &herdr::PushEvent, seq: &mut u64) -> Option<StatusEventItem> {
    use herdr::HerdrEventData::*;
    let (event_type, pane_id, agent, status) = match &ev.data {
        PaneAgentStatusChanged(c) | PaneAgentStatusChangedSub(c) => (
            "pane.agent_status_changed",
            c.pane_id.clone(),
            c.agent.clone().or_else(|| c.display_agent.clone()),
            c.agent_status.as_str().to_string(),
        ),
        PaneAgentDetected(p) => (
            "pane.agent_detected",
            p.pane_id.clone(),
            p.agent.clone(),
            p.final_status
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "unknown".into()),
        ),
        PaneExited(p) => ("pane.exited", p.pane_id.clone(), None, "exited".to_string()),
        _ => return None,
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
    let mut backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(backend.as_mut(), project)?;
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
        let resolved = resolve_agents_dyn(backend.as_mut(), &ws_id, project, to)?;
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

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agent = c
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;

    // last-sent registry: record what was delivered (or attempted). Best-
    // effort — a registry failure must never lose the send itself.
    let record = |landed: bool| {
        let rec = state::SentRecord {
            name: name.to_string(),
            text_hash: state::fnv1a64(text),
            invocation: invocation.unwrap_or_default().to_string(),
            iteration: iteration.unwrap_or(0),
            landed,
        };
        if let Err(e) = state::record_sent(project, rec) {
            eprintln!("flywheel: cannot record sent prompt: {e:#}");
        }
    };

    let Some(timeout_ms) = confirm_ms else {
        // legacy fire-and-forget path — a successful herdr call means the
        // prompt was written to the agent's terminal
        c.agent_prompt(agent_target(&agent), text, None)?;
        record(true);
        print_json(&json!({ "ok": true, "name": name }));
        return Ok(());
    };

    let wait = WaitOptions {
        timeout_ms: Some(timeout_ms),
        // landing semantics: the prompt started executing. NOT idle/done —
        // a long task must not hold the leaf for its full duration.
        until: vec![AgentStatus::Working, AgentStatus::Blocked],
    };
    let verdict = match c.agent_prompt_coded(agent_target(&agent), text, Some(wait)) {
        Ok(info) => classify_send_result(None, Some(info.agent_status)),
        Err(e) => classify_send_result(Some(&e.code), None),
    };
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
    record(landed);
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
fn resolve_pane_id(agents: &[herdr::AgentInfo], target: &str) -> Option<String> {
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
    match c.pane_wait_output(&pane_id, wait_output_mode(regex), pattern, timeout_ms) {
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
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agent = c
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
    println!("{}", agent.agent.as_deref().unwrap_or("unknown"));
    Ok(())
}

/// internal leaf: print recent output of a named agent (context-marker check).
fn cmd_internal_read(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agent = c
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
    let text = c.agent_read(agent_target(&agent), "recent", Some(60))?;
    // state-file fallback channel: parse the agent's state file when present.
    // Malformed/absent files yield null — never an error (the loop treats
    // null as "terminal channel only", identical to pre-zwa behavior).
    let state: Option<agentstate::AgentStateFile> =
        agentstate::state_file_path(&project_dir(cfg, project), agent_target(&agent))
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|content| agentstate::parse_state_file(&content));
    print_json(&json!({ "terminal": text, "state": state }));
    Ok(())
}

/// internal leaf: close the agent's pane, split a fresh pane, restart the agent
/// under the same name (fresh session context).
fn cmd_internal_respawn(cfg: &Config, project: &str, name: &str, kind: &str) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let dir = project_dir(cfg, project);
    // The agent record may already be gone (herdr drops dead agents ~0.5s
    // after they stop responding — the escalation path respawns exactly
    // those). The pane itself usually survives; split from any pane in the
    // workspace and skip closing an old pane we cannot identify.
    let agent = c
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name));
    let old_pane = agent.as_ref().map(|a| a.pane_id.clone());
    let panes = c.pane_list(Some(&ws_id))?;
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
    let new_pane = c.pane_split(&anchor, "down", Some(dir.to_str().unwrap()))?;
    if let Some(old) = &old_pane {
        let _ = c.call("pane.close", json!({ "pane_id": old }));
    }
    let fresh = start_agent_with_retry(&mut c, name, kind, &new_pane.pane_id, &[])?;
    // pane-identity bridge bookkeeping (best-effort — never fail the respawn
    // on mail trouble): the old pane died (its identity file is stale), the
    // fresh pane must resolve to the same name (the agent keeps its identity)
    if let Ok(mut mail) = backend::mail_backend(cfg) {
        if let Some(key) = dir.to_str() {
            if let Some(old) = &old_pane {
                if let Err(e) = mail.clear_pane_identity(key, old) {
                    eprintln!("flywheel: mail clear_pane_identity failed for {old}: {e}");
                }
            }
            if let Err(e) = mail.write_pane_identity(key, &fresh.pane_id, &agent_mail_name(name)) {
                eprintln!(
                    "flywheel: mail write_pane_identity failed for {name} on {}: {e}",
                    fresh.pane_id
                );
            }
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
    let mut mail = match mail::Mail::from_config(cfg.agent_mail.as_ref()) {
        Ok(Some(m)) => m,
        Ok(None) | Err(_) => {
            print_json(&json!({ "items": [] }));
            return Ok(());
        }
    };
    let key = project_dir(cfg, project).to_str().unwrap().to_string();
    if mail.ensure_project(&key).is_err() {
        print_json(&json!({ "items": [] }));
        return Ok(());
    }
    let name = mail.name.clone();
    let filter = mail::MailInboxFilter {
        urgent_only: false,
        unread_only: false,
        ack_overdue_only: false,
        include_bodies: true,
        limit,
        since_ts: None,
    };
    let v = match mail.inbox_filtered(&key, &name, &filter) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("flywheel: xproj-inbox fetch failed: {e:#}");
            print_json(&json!({ "items": [] }));
            return Ok(());
        }
    };
    let items = extract_xproj_from_inbox(&v);
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
    message_id: u64,
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
            false, // background workspace
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
    if let Ok(mut mail) = backend::mail_backend(cfg) {
        let key = project_dir(cfg, project)
            .to_str()
            .unwrap_or(project)
            .to_string();
        let _ = mail.ensure_project(&key);
        let _ = mail.send(
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
    let mut mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for blocked alert to {name}: {e:#}");
            print_json(&json!({ "ok": false, "notified": name, "error": format!("{e:#}") }));
            return Ok(());
        }
    };
    let key = project_dir(cfg, project).to_str().unwrap().to_string();
    // the recipient is the agent's REGISTERED agent-mail identity (bd-odc) —
    // `name` is the herdr name, which is not a registrable identity
    let mail_target = resolve_mail_target(name);
    let thread = format!("health-{name}");
    let out = mail.ensure_project(&key).and_then(|_| match tier {
        0 => mail.send(
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] you are blocked",
            "The swarm loop detected you are blocked. Resolve or report the blocker.",
            "high",
            true,
            Some(&thread),
        ),
        1 => mail.send(
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] still blocked — 2nd ping (high)",
            "Still blocked after a full interval. Please resolve or report the blocker so the loop can continue.",
            "high",
            true,
            Some(&thread),
        ),
        2 => mail.send(
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] URGENT: blocked — escalation",
            "Still blocked after multiple pings. Escalating; human attention requested.",
            "urgent",
            true,
            Some(&thread),
        ),
        _ => mail.send(
            &key,
            std::slice::from_ref(&mail_target),
            "[flywheel] HALTS: blocked too long — human required",
            "The swarm loop is halting because this agent stayed blocked past the escalation budget. Resolve the blocker, then re-run the loop.",
            "urgent",
            true,
            Some(&thread),
        ),
    });
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
/// Soft-fail always: exit 0 with `{ok, ...}` so overnight loops never die.
fn cmd_internal_diagnose(cfg: &Config, project: &str, name: &str, reason: &str) -> Result<()> {
    // --- tail (best-effort) ---
    let mut tail = String::new();
    let mut backend = match backend::multiplexer_backend(cfg) {
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
        Ok(mut mail) => {
            let key = project_dir(cfg, project).to_str().unwrap().to_string();
            let subject = format!("[{project}] {name} unresponsive ({reason})");
            let body = format!(
                "Agent **{name}** is unresponsive (`{reason}`).\n\n\
                 Project: `{project}`\n\
                 Thread: `health-{name}`\n\n\
                 ### Recent output (truncated)\n```\n{tail}\n```\n\n\
                 flywheel will attempt respawn if the agent stays dead (budgeted)."
            );
            match mail.ensure_project(&key).and_then(|_| {
                mail.send(
                    &key,
                    std::slice::from_ref(&recipient),
                    &subject,
                    &body,
                    "high",
                    false,
                    Some(&format!("health-{name}")),
                )
            }) {
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
    let mut mail = backend::mail_backend(cfg)?;
    let key = project_dir(cfg, project).to_str().unwrap().to_string();
    mail.ensure_project(&key)?;
    // NOTE: MailBackend::name() returns the backend identifier ("agentmail"),
    // not the orchestrator's agent-mail identity — use the configured name so
    // the reported `agent` matches the reservations' owner.
    let agent = orchestrator_mail_name(cfg);
    let v = mail
        .renew(&key, &agent, extend_seconds, None)
        .map_err(|e| anyhow!("mail backend: {e}"))?;
    let renewed = v.get("renewed").and_then(Value::as_u64).unwrap_or(0);
    let reservations = crate::mail::list_own_reservations(&v);
    print_json(&json!({
        "ok": true,
        "renewed": renewed,
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
    use crate::herdr::{AgentInfo, PaneWaitResult};

    fn agent(name: Option<&str>, pane: &str) -> AgentInfo {
        AgentInfo {
            agent: Some("opencode".into()),
            agent_session: None,
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
        let (_a, _b, j) =
            resolve_debate_agents(&Config::default(), "p", None, None, None, "t").unwrap();
        assert_eq!(j, "gk"); // judge defaults to grok
    }

    #[test]
    fn debate_rejects_same_agent_for_a_and_b() {
        let r = resolve_debate_agents(&Config::default(), "p", Some("oc"), Some("oc"), None, "t");
        assert!(r.is_err(), "A and B must be distinct");
    }

    #[test]
    fn debate_accepts_explicit_agents() {
        let (a, b, j) = resolve_debate_agents(
            &Config::default(),
            "p",
            Some("pi"),
            Some("gk"),
            Some("oc"),
            "t",
        )
        .unwrap();
        assert_eq!(a, "pi");
        assert_eq!(b, "gk");
        assert_eq!(j, "oc");
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

    #[test]
    fn parse_xproj_recipe_in_subject_line() {
        let t = "[xproj] run 2-worker recipe=daily-p6\nbody line";
        let r = parse_xproj_request(t).expect("parse");
        assert_eq!(r.recipe, "daily-p6");
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

//! flywheel — multi-agent swarm orchestrator on the herdr backend.
//!
//! NTM-parity daily loop for opencode/pi/grok: spawn swarms, dispatch work,
//! monitor states, checkpoint progress. All tracker state lives in br/bv;
//! all coordination lives in agent-mail; herdr is the backend.

mod agentstate;
mod backend;
mod builtin_workflows;
mod coalition;
mod config;
mod crew;
mod cron;
mod decide;
mod durable;
mod fs_util;
mod headless;
mod herdr_shim;
mod intent;
mod mail_shim;
mod mcp_http;
mod models;
mod output;
mod pane;
mod preflight;
mod profile;
mod run_log;
mod smart_cron;
mod stages;
mod state;
mod toron_embed;
mod verdict;
mod work;
mod workflow_catalog;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use config::{Config, Recipe, WorkflowStep};
use herdr_shim::{
    AgentInfo, AgentStatus, Client, SendVerdict, Subscription, WaitOptions, alt_screen_read_ladder,
    classify_send_result, sanitize_pane_text,
};
use output::{die, dim, error, kind_paint, ok as paint_ok, print_json, status_paint};
use profile::ProfileCatalog;
use state::{EventKind, log_event};
use toron_core::backend::error::BackendErrorCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---- stage prompts (ADR-0005 Spec 2a / P3.3) ----
//
// Resolved through the `stages` library: compiled-in defaults (the former
// STAGES const, verbatim) under semantic names + p-code aliases, overridable
// by ~/.config/flywheel/stages.toml. A `--stage` name may be a semantic name
// (`systematic-execute`) OR a p-code alias (`p6`) — both resolve to the same
// prompt (the stage_alias contract).

pub(crate) fn stage_prompt(name: &str) -> Result<String> {
    let lib = stages::StageLibrary::load()?;
    lib.resolve(name).map(|p| p.to_string()).ok_or_else(|| {
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

    /// Skip preflight checks (steer/loop/workflow auto-verify before dispatch)
    #[arg(long, global = true)]
    skip_preflight: bool,

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
        /// skip the post-spawn visibility check (scripts; not recommended —
        /// a silent registry-write failure leaves panes invisible to list/send)
        #[arg(long)]
        no_verify: bool,
        /// close pre-existing FLYWHEEL-OWNED workspaces for this project
        /// (stale panes from a killed swarm race the new one —
        /// bd-fw-stale-workspace-panes-hgp + bd-627z.3). Without this, spawn
        /// REFUSES when one exists; foreign workspaces (captain/omp sessions)
        /// are never closed — they only warn.
        #[arg(long)]
        retire_orphans: bool,
        /// spawn agents with the peer-aware self-poll template (ADR-0001 §9.3)
        /// — DEFAULT ON (2026-08-15, captain: collab is not optional; manual
        /// steering is not a swarm). --no-peer-aware opts out for single-shot
        /// panes.
        #[arg(long, action = clap::ArgAction::SetTrue, default_value_t = true)]
        peer_aware: bool,
        /// opt OUT of the peer-aware self-poll template (single-shot panes)
        #[arg(long)]
        no_peer_aware: bool,
        /// peer-aware poll cadence (minutes idle between inbox polls; default 5)
        #[arg(long, default_value_t = 5)]
        peer_poll_interval_minutes: u32,
        /// peer-aware liveness budget (minutes alive; default 30)
        #[arg(long, default_value_t = 30)]
        peer_poll_budget_minutes: u32,
        /// peer-aware max peer-prompts per task (default 3)
        #[arg(long, default_value_t = 3)]
        peer_max_prompts: u32,
        /// post-spawn settle: seconds to sleep after the readiness gate so a
        /// booted-but-not-input-ready TUI (MCP init, model connect) catches
        /// prompts (bd-eign.6). Default 30 — observed boot is 20-30s.
        #[arg(long, default_value_t = 30)]
        settle_secs: u64,
        /// spawn agents from a profile (ADR-0005, bd-eign.13): the profile's
        /// harness determines the kind, its model/effort ride the launch args
        /// (bd-eign.9), and its behavior/skills condition dispatches via the
        /// spawn registry. Combinable with --oc/--pi/--gk/--kind for mixed
        /// crews.
        #[arg(long)]
        profile: Option<String>,
        /// how many profile agents to spawn (with --profile; default 1)
        #[arg(long, default_value_t = 1)]
        count: u32,
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
    /// Classify a target into one captain primitive (no dispatch)
    Decide {
        /// free-text target / intent
        target: String,
        /// ready-bead count from `br ready`
        #[arg(long, default_value_t = 0)]
        ready_beads: u32,
        /// profiled panes already up
        #[arg(long, default_value_t = 0)]
        profiled_panes: u32,
        /// agents still launch_pending
        #[arg(long, default_value_t = 0)]
        launch_pending: u32,
        /// file-contention signal
        #[arg(long)]
        contention: bool,
        /// machine JSON (primitive, reason, argv)
        #[arg(long)]
        json: bool,
    },
    /// Restore lost agent name bindings from the spawn registry
    /// (bd-fw-name-binding-loss-7rc): panes whose agent restarted show
    /// name:null — this renames them back to their registered names.
    Rebind { project: String },
    /// Recover a damaged swarm (bd-fw-interrupt-kills-agent-or3): for each
    /// registered agent, restore the name binding if the pane survived, or
    /// respawn the agent if it died. The durable loop self-heals on its own;
    /// this is the CLI supervision counterpart.
    Recover { project: String },
    /// Send a prompt to one or more agents (with optional wait)
    Send {
        project: String,
        /// prompt text (or --stage for the daily-driver stages)
        msg: Option<String>,
        /// target agents: comma-separated shorts (oc,pi,gk), full names, or "all"
        /// (bare shorts reach the whole crew of a kind — oc hits oc,oc2,oc3;
        /// a full name like flywheel-flywheel-oc or a numbered short like oc2
        /// matches EXACTLY one agent)
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
        /// bd-rdy: on agent_prompt_stalled (after one automatic retry),
        /// respawn the agent under the same name and re-send instead of
        /// failing with recovery guidance
        #[arg(long)]
        respawn_on_stall: bool,
    },
    /// Interrupt an agent (sends Escape by default — cancels the current
    /// tool call and returns to the prompt). Kill-style keys (ctrl+c) are
    /// REFUSED unless --force: they terminate the agent process and drop the
    /// pane to a shell (bd-fw-interrupt-kills-agent-or3).
    Interrupt {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
        /// key names to send instead of Escape (repeatable); ctrl+c requires
        /// --force
        #[arg(long)]
        keys: Vec<String>,
        /// allow kill-style keys (ctrl+c terminates the agent process)
        #[arg(long)]
        force: bool,
    },
    /// Read an agent's terminal output
    View {
        project: String,
        #[arg(long, default_value = "oc")]
        to: String,
        #[arg(long)]
        lines: Option<u32>,
        #[arg(long, default_value = "recent_unwrapped")]
        source: String,
    },
    /// Read an agent's output from the opencode structured session store
    /// (bd-eign.5) — the SQLite session log, not TUI scrollback (which the
    /// alternate screen garbles). Resolves the latest session for the
    /// project directory, optionally narrowed by opencode role; --session
    /// targets an exact session id.
    Output {
        project: String,
        /// opencode role filter (build/explore/general/beads/plan)
        #[arg(long)]
        role: Option<String>,
        /// exact session id (skips directory/role resolution)
        #[arg(long)]
        session: Option<String>,
        /// max parts to read from the session
        #[arg(long, default_value = "200")]
        limit: u32,
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
    /// crashes resume via `flywheel loop run <project>` (same invocation id).
    /// For back-compat, the bare legacy form `flywheel loop <project>` is
    /// rewritten to `loop run <project>` before parsing (see
    /// `rewrite_legacy_loop_args`).
    Loop {
        #[command(subcommand)]
        cmd: LoopCmd,
    },
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
        /// required when any agent is still `working` (cargo test / long
        /// jobs routinely exceed 15m — a plain --force used to reap them)
        #[arg(long)]
        force_working: bool,
    },
    /// Work-graph surfaces (bv/br pass-through)
    Work {
        #[command(subcommand)]
        cmd: WorkCmd,
    },
    /// Assign beads to agents via mail (toron default)
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
    /// toron mail coordination surfaces
    Mail {
        #[command(subcommand)]
        cmd: MailCmd,
    },
    /// ask a peer a question over mail and wait for the in-thread reply
    /// (the real-time collab primitive: send urgent + block for the answer)
    Collab {
        project: String,
        /// peer agent (adjective+noun or herdr short name)
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        /// seconds to wait for the reply (default 300; 0 = forever)
        #[arg(long, default_value = "300")]
        timeout_secs: u64,
        /// act as this agent (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
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
    /// Generate the beads-agent integration prompts for an agent kind.
    /// Writes 5 files (beads-{triage,execute,review,commit,create}.md) into
    /// the tool's prompts dir. Omit KIND to list the available kinds.
    /// Re-run after a flywheel rebuild to refresh the generated prompts.
    Integration {
        /// agent kind: opencode|pi|grok|omp (shorts oc|pi|gk accepted);
        /// omit to list kinds
        kind: Option<String>,
        /// write the 5 prompt files into this directory instead of the
        /// tool's conventional prompts dir
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
    /// Coalitions (ADR-0011 Spec 1, P11.3a) — captain-formed crews around a
    /// goal: NIP-29 channel + goal run + intent board + manual request-release
    /// collab. Steer surface without the auto-proposer (P11.3b).
    Coalition {
        #[command(subcommand)]
        cmd: CoalitionCmd,
    },
    /// Runs dashboard (ADR-0005 P6.3) — state + owner + per-run cost
    Runs {
        #[command(subcommand)]
        cmd: RunsCmd,
    },
    /// Captain-free steering (bd-t86p): ONE compose call at a target —
    /// `flywheel steer "<objective>"` drives the crew to completion with
    /// zero manual per-bead dispatch. Bead-tracked by default (goal →
    /// brainstorm → plan → beads → build → review → ship); `--direct` skips
    /// the tracker (non-bead targets: the plan flows straight to the coder,
    /// no approval gate — truly autonomous).
    Steer {
        /// the objective/target ("implement the auth refactor")
        objective: String,
        /// direct-dispatch arm: skip bead tracking + the plan approval gate
        /// (non-bead targets)
        #[arg(long)]
        direct: bool,
        /// crew strategy: auto | <squad> (default auto)
        #[arg(long)]
        crew: Option<String>,
        /// pane mode: the project whose workspace hosts the crew
        #[arg(long)]
        project: Option<String>,
        /// pane mode: bind to an existing workspace id instead of spawning
        #[arg(long)]
        reuse: Option<String>,
        /// pane mode: keep the crew workspace after the run (default: reap)
        #[arg(long)]
        retain: bool,
        /// pane mode: close pre-existing workspaces before spawning
        #[arg(long)]
        retire_orphans: bool,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Unified workflow surface (ADR-0005 Spec 5 / P3.7) — catalog, lint,
    /// run, scaffold, history, and auto-crew preview. One vocabulary, one
    /// engine: built-ins + canonical `workflows/*.yaml` + absorbed legacy
    /// `workflows.toml` / `pipelines.toml`.
    Workflow {
        #[command(subcommand)]
        cmd: WorkflowCmd,
    },
    /// Model registry (ADR-0010 Spec D, P10.2) — `models.toml` ids with
    /// provider/cost/caps/fallback; Profile.model resolves against it so the
    /// per-harness model flag is real, not advisory.
    Model {
        #[command(subcommand)]
        cmd: ModelCmd,
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
    /// Preflight: verify all prerequisites for a fleet operation before
    /// dispatching. Catches environment/dependency failures in <1s instead
    /// of 30s+ into a Resonate cycle. Auto-runs before steer/loop/workflow
    /// unless --skip-preflight is passed.
    Preflight {
        /// operation mode to check: steer|loop|workflow|cron (default loop)
        #[arg(long, default_value = "loop")]
        mode: String,
        /// project path
        #[arg(long)]
        project: Option<String>,
        /// loop --work mode (adds beads check)
        #[arg(long)]
        work: bool,
        /// JSON output
        #[arg(long)]
        json: bool,
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
        /// profile name whose behavior preamble conditions the prompt
        /// (Spec C, bd-gsvn.1). Empty = raw text (plain swarm sends).
        #[arg(long)]
        profile: Option<String>,
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
    /// internal: mail the planner that a ready bead is NOT dispatchable
    /// (missing/malformed `## VERIFY` fence — ADR-0019, bd-w1-verify-fence)
    #[command(name = "__mail-not-ready", hide = true)]
    __MailNotReady {
        project: String,
        bead_id: String,
        title: String,
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
    /// internal: mail digest for one agent (unread counts + last sender/subject)
    #[command(name = "__mail-digest", hide = true)]
    __MailDigest { project: String, agent: String },
    /// internal: newest message id + author from a thread BY a sender
    /// (bd-a7j reply verification — did the thread move after dispatch?)
    #[command(name = "__check-reply", hide = true)]
    __CheckReply {
        project: String,
        name: String,
        thread: String,
    },
    /// internal: the project's expected-reply entries (checkpointed leaf —
    /// collab may have written new ones while the loop runs)
    #[command(name = "__expected-replies", hide = true)]
    __ExpectedReplies { project: String },
    /// internal: log ReplyMissing + dispatch the concrete reminder send
    /// (one checkpointed leaf; the send itself recurses into __send)
    #[command(name = "__remind-reply", hide = true)]
    __RemindReply {
        project: String,
        name: String,
        thread: String,
        /// reminder ordinal (0 = first)
        #[arg(long, default_value_t = 0)]
        reminder_count: u32,
        #[arg(long)]
        invocation: Option<String>,
        #[arg(long)]
        iteration: Option<u64>,
    },
    /// internal: drop a satisfied expected-reply entry (checkpointed leaf so
    /// the workflow body never writes files — bd-a7j)
    #[command(name = "__reply-settled", hide = true)]
    __ReplySettled {
        project: String,
        name: String,
        thread: String,
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
    /// internal: release every live file reservation whose reason is the
    /// bead id (bd-h3oj release-on-bead-close; durable workflow leaf —
    /// idempotent, `released == 0` is a clean no-op)
    #[command(name = "__release-bead-locks", hide = true)]
    __ReleaseBeadLocks { project: String, bead_id: String },
    /// internal: list ready beads as JSON (durable workflow leaf)
    #[command(name = "__ready", hide = true)]
    __Ready { project: String },
    /// internal: list in-progress beads as JSON (durable workflow leaf)
    #[command(name = "__inprogress", hide = true)]
    __Inprogress { project: String },
    /// internal: reclaim stranded in-progress beads (bd-627z.1) — re-open
    /// each whose assignee is not in the live agent list (durable workflow
    /// leaf; emits `{"reclaimed": [ids], "failed": [{id, error}]}`)
    #[command(name = "__reclaim-stranded", hide = true)]
    __ReclaimStranded {
        project: String,
        /// comma-separated live agent names (the polled swarm)
        #[arg(long)]
        live: String,
    },
    /// internal: agent kind as text (durable workflow leaf)
    #[command(name = "__kind", hide = true)]
    __Kind { project: String, name: String },
    /// internal: profile name for an agent from the spawn registry
    /// (durable workflow leaf, bd-gsvn.1 — empty when none)
    #[command(name = "__profile", hide = true)]
    __Profile { project: String, name: String },
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
    /// internal: run one prompt headlessly through a harness (ADR-0010
    /// Spec A/E leaf). The RUNNER preprends the role preamble — the leaf
    /// stays dumb: resolve profile → harness, run, log, exit.
    #[command(name = "__run-headless", hide = true)]
    __RunHeadless {
        /// profile name (`planner` or `@planner`)
        profile: String,
        /// the (already role-conditioned) prompt
        prompt: String,
        /// model override (defaults to the profile's model)
        #[arg(long)]
        model: Option<String>,
        /// per-step harness override (bd-42fs step 5): pi|omp|opencode|grok
        /// — overrides the profile's harness for THIS step only
        #[arg(long)]
        harness: Option<String>,
        /// owning workflow name (run-log schema)
        #[arg(long)]
        workflow: Option<String>,
        /// owning cron name (P10.4 run-log attribution; `cron history` reads it)
        #[arg(long)]
        cron: Option<String>,
        /// one JSON line for the parent runner (default: human summary)
        #[arg(long)]
        json: bool,
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

/// Subcommands of `flywheel loop` (bd-fw-stale-loop-processes-gac
/// restructure): `run` (default), `status`, `list`, `prune`.
#[derive(Subcommand)]
enum LoopCmd {
    /// Run the monitoring loop for a project (default subcommand — the bare
    /// legacy form `flywheel loop <project>` is rewritten to this before
    /// parsing, so both spellings work).
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
        /// run the loop as a WorkflowEngine durable function (bd-e0xk
        /// coexistence path): dispatches run as `loop-dispatch` engine
        /// workflows instead of the imperative body's send leaf
        #[arg(long)]
        engine: bool,
        /// take over the project from a live loop process instead of refusing
        #[arg(long)]
        force: bool,
    },
    /// Query the state of a durable loop invocation on the Resonate server
    Status { project: String },
    /// List registered loop processes (pid registry) with live/dead status
    List,
    /// Remove pid-registry entries whose process is dead (stale cleanup)
    Prune,
    /// Infrastructure diagnostics + repair (bd-eign.4): Resonate connectivity,
    /// stale pending loop invocations, orphaned loop PIDs, loops.json
    /// consistency. Human report by default, --json for machine. --fix rejects
    /// stale invocations + prunes orphaned pid entries.
    Doctor {
        /// auto-repair: reject stale invocations, prune orphaned pids
        #[arg(long)]
        fix: bool,
        /// machine-readable JSON health report
        #[arg(long)]
        json: bool,
    },
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
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// reply to a message in its thread (thread = the message id or thread id)
    Reply {
        project: String,
        /// message id to reply to (toron hex event id / thread id)
        #[arg(long = "message-id")]
        message_id: String,
        #[arg(long)]
        body: String,
        /// act as this agent (required for crew peer mail; else CoralFinch/config)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// mark a message read (read receipt; the projection honors it once
    /// read-state lands toron-side — bd-5icg)
    Read {
        project: String,
        /// message id (toron hex event id)
        #[arg(long = "message-id")]
        message_id: String,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// acknowledge a message that had ack_required=true
    Ack {
        project: String,
        /// message id (toron hex event id)
        #[arg(long = "message-id")]
        message_id: String,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// FTS5 search over the project's indexed messages
    Search {
        project: String,
        /// FTS5 query (phrase/prefix/boolean; hyphenated ids are quoted)
        query: String,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// filter by sender (adjective+noun or herdr name)
        #[arg(long)]
        sender: Option<String>,
        /// filter by importance: low|normal|high|urgent
        #[arg(long)]
        importance: Option<String>,
        /// filter to one thread (thread id or message id)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        /// newest first instead of relevance
        #[arg(long)]
        recency: bool,
        /// include message bodies in results
        #[arg(long)]
        bodies: bool,
    },
    /// summarize a thread (participants, key points, action items)
    Summarize {
        project: String,
        /// thread id or message id
        #[arg(long = "thread-id")]
        thread_id: String,
        /// include up to 3 sample messages
        #[arg(long)]
        examples: bool,
    },
    /// resolve an agent's profile (npub, program, model, task)
    Whois {
        project: String,
        /// agent (adjective+noun or herdr short name)
        agent: String,
    },
    /// register/refresh an agent identity (upsert)
    Register {
        project: String,
        /// agent name (adjective+noun; omit to auto-generate)
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "flywheel")]
        program: String,
        #[arg(long, default_value = "flywheel")]
        model: String,
        /// short task description shown in listings
        #[arg(long)]
        task: Option<String>,
    },
    /// renew this identity's file reservations (extend the TTL)
    Renew {
        project: String,
        /// seconds to extend from the current expiry (default 3600)
        #[arg(long, default_value = "3600")]
        extend_seconds: u64,
        /// restrict to these path patterns (default: all of this identity's)
        #[arg(long)]
        paths: Vec<String>,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// operator force-release of a stale reservation (herdr-down / abandoned)
    ForceUnlock {
        project: String,
        /// path (or glob) of the stale reservation to revoke — the daemon
        /// resolves the active lease on this path itself (bd-cumj)
        #[arg(long = "paths", value_name = "PATH")]
        path: String,
        /// holder agent name to disambiguate (optional)
        #[arg(long)]
        holder: Option<String>,
        /// optional note for the released holder
        #[arg(long)]
        note: Option<String>,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// build slots: acquire/renew/release the advisory 'cargo' slot
    Slot {
        #[command(subcommand)]
        cmd: MailSlotCmd,
    },
    /// set this agent's inbound contact policy
    Policy {
        project: String,
        /// policy: open|auto|contacts_only|block_all
        #[arg(long)]
        policy: String,
        /// agent whose policy to set (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// per-agent mail digest: unread/urgent counts + last sender/subject
    /// (orchestrator "who has mail" view; default = own identity)
    Digest {
        project: String,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// digest every registered agent (table, --json = array)
        #[arg(long)]
        all: bool,
    },
    /// wait until a thread gains a new message (blocks; Ctrl-C to abort)
    Watch {
        project: String,
        /// thread id or message id to watch
        #[arg(long = "thread")]
        thread_id: String,
        /// act as this registered agent (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// seconds to wait before giving up (default 300; 0 = forever)
        #[arg(long, default_value = "300")]
        timeout_secs: u64,
        /// poll interval seconds (default 5)
        #[arg(long, default_value = "5")]
        interval_secs: u64,
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
    /// reserve / inspect file locks (advisory; subcommands: reserve, list, show)
    Locks {
        #[command(subcommand)]
        cmd: MailLockCmd,
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
    /// list registered agents in the project (active within 24h by default;
    /// --all shows the full history incl. stale registrations)
    Agents {
        project: String,
        /// include ALL registered identities (history), not just active ones
        #[arg(long)]
        all: bool,
    },
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
enum MailLockCmd {
    /// reserve files (advisory locks)
    Reserve {
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
    /// dump the active lock board (path, holder, exclusive, reason, expires, id)
    List {
        project: String,
        /// filter to one holder's leases
        #[arg(long)]
        holder: Option<String>,
    },
    /// show the holders of one path/glob (glob-overlap match)
    Show {
        project: String,
        /// path or glob whose holders to show
        path: String,
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
enum MailSlotCmd {
    /// acquire the advisory build slot (exclusive recommended)
    Acquire {
        project: String,
        #[arg(long, default_value = "cargo")]
        slot: String,
        #[arg(long)]
        exclusive: bool,
        #[arg(long, default_value = "1800")]
        ttl_seconds: u64,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// extend an active slot lease
    Renew {
        project: String,
        #[arg(long, default_value = "cargo")]
        slot: String,
        #[arg(long, default_value = "1800")]
        extend_seconds: u64,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// release an active slot lease (non-destructive)
    Release {
        project: String,
        #[arg(long, default_value = "cargo")]
        slot: String,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
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
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// omp role model for lightweight tasks (--smol=, bd-gsvn.2)
        #[arg(long)]
        smol: Option<String>,
        /// omp role model for slow/thorough reasoning (--slow=, bd-gsvn.2)
        #[arg(long)]
        slow: Option<String>,
        /// omp role model for architectural planning (--plan=, bd-gsvn.2)
        #[arg(long)]
        plan: Option<String>,
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
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// omp role model for lightweight tasks (--smol=, bd-gsvn.2)
        #[arg(long)]
        smol: Option<String>,
        /// omp role model for slow/thorough reasoning (--slow=, bd-gsvn.2)
        #[arg(long)]
        slow: Option<String>,
        /// omp role model for architectural planning (--plan=, bd-gsvn.2)
        #[arg(long)]
        plan: Option<String>,
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
    /// Remove a local profile from ~/.config/flywheel/profiles.toml (built-ins
    /// cannot be removed — override them via `create`/`edit` instead).
    Remove {
        /// profile name
        name: String,
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
    /// Remove a squad/recipe from ~/.config/flywheel/recipes.toml.
    Remove {
        /// squad / recipe name
        name: String,
    },
}

/// Coalition commands (ADR-0011 Spec 1, P11.3a): the steer surface + manual
/// collab. A coalition = NIP-29 channel + goal workflow run + manifest.
/// Manual collab reuses P11.2 (soft-conflict freeze + request-release) — no
/// auto-proposer here (P11.3b).
#[derive(Subcommand)]
enum CoalitionCmd {
    /// Register an agent identity in the coalition's toron project (members
    /// must be registered before `form`; profiles are not agents).
    Register {
        /// agent name (key name in the project's agents dir)
        name: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Form a captain-led coalition around a goal workflow: lint gate, start
    /// the goal run, create the NIP-29 channel, write the manifest.
    Form {
        /// goal workflow name (must pass the lint gate)
        goal: String,
        /// crew: comma-separated squads (recipes.toml) / profile names —
        /// one per project for cross-project coalitions (bd-hkw5.3)
        #[arg(long)]
        crews: String,
        /// act as this agent (the captain; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// cross-project scoping: <member>=<project> (repeatable; absent
        /// members default to --project) (bd-hkw5.3)
        #[arg(long = "member-project")]
        member_project: Vec<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// bead ids tracked for the beads-closed disband gate (comma list)
        #[arg(long)]
        beads: Option<String>,
        /// the repo whose .beads holds the tracked beads (br resolves its
        /// workspace from cwd; default: none — probe from the current dir)
        #[arg(long)]
        tracker: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Full coalition view: manifest + run + channel + intents + reservations
    /// + frozen paths + in-flight negotiations + bead gate.
    Status {
        /// coalition id (from form / list)
        id: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// compact one-line-per-member digest (bd-hkw5.1)
        #[arg(long)]
        digest: bool,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Declare status:intent into the coalition's intent board (P11.2).
    Intent {
        /// coalition id
        id: String,
        /// path globs the intent covers
        paths: Vec<String>,
        /// act as this agent (must be a member)
        #[arg(long = "as")]
        as_agent: String,
        #[arg(long)]
        reason: Option<String>,
        /// intent TTL in seconds (default 300)
        #[arg(long)]
        ttl: Option<u64>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Acquire leases on path globs (P11.2). A soft-conflict FREEZE against
    /// another member's intent lands in soft_conflicts — the manual-collab
    /// trigger.
    Reserve {
        /// coalition id
        id: String,
        /// path globs to reserve
        paths: Vec<String>,
        /// act as this agent (must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// lease TTL in seconds
        #[arg(long)]
        ttl: Option<u64>,
        /// exclusive lease (default: true)
        #[arg(long)]
        exclusive: bool,
        #[arg(long)]
        reason: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: ask the holder to release/defer (P11.2).
    NegotiateRequest {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the holder being asked (must be a member)
        #[arg(long)]
        target: String,
        /// reservation id (kind-30031 event id from reserve)
        #[arg(long)]
        reservation_id: String,
        /// ask: `release` | `defer:<secs>`
        #[arg(long)]
        ask: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: the holder answers (P11.2).
    NegotiateRespond {
        /// coalition id
        id: String,
        /// act as this agent (the holder; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        /// decision: `agreed` | `denied` | `deferred:<secs>`
        #[arg(long)]
        decision: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: negotiation resolution (pending / responded /
    /// escalation-due) (P11.2).
    NegotiateStatus {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        /// escalation tier window in seconds (default 900)
        #[arg(long)]
        window: Option<u64>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: escalate an unanswered request to the captain
    /// (kind-14 DM, P11.2).
    NegotiateEscalate {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// List coalitions (id, goal, status).
    List {
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// P11.3b detector: scan the intent board for overlapping crews and
    /// propose tiered rule-splits (split-by-directory / sequence-by-
    /// dependency / merge) + flag auto-coalesce candidates (non-trivial
    /// source overlaps between unaffiliated crews).
    Detect {
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// P11.3b auto-coalesce: form an ad-hoc coalition around a goal workflow
    /// for every non-trivial overlap the detector flagged between
    /// unaffiliated crews.
    AutoCoalesce {
        /// goal workflow name (must pass the lint gate)
        goal: String,
        /// act as this agent (the captain; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Disband a coalition: beads-closed gate (when tracked), member
    /// reservations released, channel close post, manifest closed.
    Disband {
        /// coalition id
        id: String,
        /// require every tracked bead to be closed first (the primary
        /// disband path; manual disband is the auto-fallback)
        #[arg(long)]
        beads_closed: bool,
        #[arg(long)]
        reason: Option<String>,
        /// override the tracker repo recorded at form (br cwd for the
        /// beads-closed probe)
        #[arg(long)]
        tracker: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Dispatch a headless leaf worker (bd-hkw5.2): the captain pre-splits
    /// a non-overlapping scope; the leaf runs it one-shot (ADR-0010 Spec A
    /// subprocess-reuse), logs to runs/, and posts the result to the
    /// coalition channel. Leaf semantics — no intent-declare/negotiate.
    Leaf {
        /// coalition id
        id: String,
        /// the acting member (leaf worker; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// profile ref the leaf runs under (@planner)
        #[arg(long)]
        profile: String,
        /// the captain's pre-split, non-overlapping scope
        #[arg(long)]
        scope: String,
        /// per-dispatch harness override (pi|omp|opencode|grok)
        #[arg(long)]
        harness: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
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
        /// real execution (ADR-0010 Spec A): without it the action runner
        /// records only — control-flow runs, side-effects are NOT dispatched
        /// (the backward-compat default).
        #[arg(long)]
        execute: bool,
        /// execution mode with --execute (ADR-0010 Spec E): `headless` runs
        /// steps via the `__run-headless` subprocess; `pane` pre-spawns a
        /// real herdr crew and dispatches via the loop's `__send` leaf
        /// (P10.3, bd-ghcj.1).
        #[arg(long, value_parser = ["headless", "pane"], default_value = "headless")]
        mode: String,
        /// pane mode: the project whose workspace hosts the crew (default
        /// `flywheel` — the flywheel project dir must exist)
        #[arg(long)]
        project: Option<String>,
        /// pane mode: bind to this existing workspace id instead of spawning
        /// a fresh one (crew must already be running)
        #[arg(long)]
        reuse: Option<String>,
        /// pane mode: keep the crew workspace after the run (default: reap —
        /// close the workspace + its panes at run end)
        #[arg(long)]
        retain: bool,
        /// pane mode: close pre-existing workspaces of the project before
        /// spawning the crew (stale-pane protection, same contract as spawn)
        #[arg(long)]
        retire_orphans: bool,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Scaffold a new workflow YAML in the config dir
    New {
        /// workflow name (must be a valid identifier)
        name: String,
    },
    /// Runtime workflow composition (bd-8xca): assemble a NEW workflow from
    /// existing parts — `role:stage` dispatch parts and `workflow:name`
    /// call_workflow parts — around a target, lint it, and (with --run)
    /// execute it durably. The deterministic arm (default) assembles from
    /// the parts spec; `--agent` lets a planner agent author the workflow
    /// from the target + catalog.
    Compose {
        /// the objective/target the composed workflow executes
        target: String,
        /// optional base workflow whose steps seed the composition
        #[arg(long)]
        base: Option<String>,
        /// parts spec: comma-separated `role:stage` or `workflow:name`
        #[arg(long, value_delimiter = ',')]
        parts: Vec<String>,
        /// planner arm: an agent authors the workflow from the target
        #[arg(long)]
        agent: bool,
        /// run the composed workflow (lint gate first, pane mode)
        #[arg(long)]
        run: bool,
        /// pane mode: the project whose workspace hosts the crew
        #[arg(long)]
        project: Option<String>,
        /// JSON output
        #[arg(long)]
        json: bool,
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
    /// Remove a catalog workflow (deletes ~/.config/flywheel/workflows/<name>.yaml).
    /// Built-in workflows cannot be removed — override via a catalog file of the
    /// same name (catalog shadows built-ins).
    Remove {
        /// workflow name (catalog only)
        name: String,
    },
}

/// `flywheel model` subcommands (ADR-0010 Spec D, P10.2): the `models.toml`
/// registry. Profile.model ids resolve against it for per-harness injection;
/// unknown ids warn and pass through (never block a run).
#[derive(Subcommand)]
enum ModelCmd {
    /// List registered model ids
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Show one registry entry (provider, cost, caps, fallback)
    Show {
        /// model id (e.g. claude-sonnet-4)
        id: String,
    },
    /// Add or update a registry entry in ~/.config/flywheel/models.toml
    Add {
        /// model id
        id: String,
        /// provider prefix (`anthropic` → opencode -m anthropic/<id>)
        #[arg(long)]
        provider: String,
        /// input cost per million tokens (advisory)
        #[arg(long, default_value_t = 0.0)]
        cost_in_per_mtok: f64,
        /// output cost per million tokens (advisory)
        #[arg(long, default_value_t = 0.0)]
        cost_out_per_mtok: f64,
        /// harnesses this model supports (comma list; empty = all)
        #[arg(long)]
        caps: Option<String>,
        /// fallback id when a harness is outside caps — ALSO the retry-on-429
        /// fallback (bd-42fs)
        #[arg(long)]
        fallback: Option<String>,
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
    },
    /// Remove a registry entry
    Remove {
        /// model id
        id: String,
    },
}

/// `flywheel cron` subcommands (ADR-0012 Spec 1/4, P12.3): the standalone
/// cron surface over cron YAMLs. Crons are YAML files in three tiers —
/// per-project > global > built-in — with a shared CronSpec body on the
/// toron-workflow engine. `lint`/`run` validate + test a cron on demand;
/// `start` drives the durable loops; pause/resume/remove mutate the tier's
/// YAML (built-ins are read-only; shadow by a user YAML instead).
#[derive(Subcommand)]
enum CronCmd {
    /// List resolved crons (per-project > global > built-in) with source +
    /// scope
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one cron's resolved definition (spec + source + scope)
    Show {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Lint cron YAML(s): engine tier (schedule parse, policy sanity, step/
    /// graph checks) + flywheel tier (owner-profile resolution). Exit 0
    /// clean, 1 on any blocking issue, 2 on a parse failure.
    Lint {
        /// lint only this cron (name), else every resolved cron
        name: Option<String>,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Run a cron's body on demand (testing): lint gate first, then the spec
    /// steps through the shared engine with the spec's mode runner.
    Run {
        /// cron name (resolved tier)
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Pause a cron (entry preserved; nothing fires) — persists `enabled:
    /// false` in the tier's YAML
    Pause {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Resume a paused cron (removes the `enabled: false` marker)
    Resume {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Status of every cron: paused | due | running | succeeded | failed
    Status {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Run history for a cron (state-log scan + failure beads)
    History {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Start (or resume) the durable cron loops over the resolved crons.
    /// Health-gate probes + `on_failure: bead` are live. Blocks; Ctrl-C to
    /// stop.
    Start {
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Remove a cron definition (user tiers only; built-ins are read-only).
    /// Removing a project-tier cron reveals the global/built-in definition
    /// it shadowed.
    Remove {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
}

fn parse_kind_count(s: &str) -> Result<(String, u32)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("expected KIND=COUNT, got {s}"))?;
    Ok((k.to_string(), v.parse()?))
}

// ---- swarm helpers ----

pub(crate) fn project_dir(cfg: &Config, project: &str) -> PathBuf {
    let p = PathBuf::from(project);
    if p.is_absolute() {
        p
    } else {
        cfg.workspace_root().join(project)
    }
    // NOTE (2026-08-15, bd-test-tmp-hygiene-pq5b): a RELATIVE project key
    // resolves to a data dir under the workspace root — gate/test invocations
    // must pass ABSOLUTE scratch paths (mktemp -d / temp_dir), or the test
    // matrix leaks mailbox-archive dirs into the workspace. verify-workspace-
    // clean.sh enforces this.
}

/// Fallible UTF-8 conversion of a filesystem path (193.1). Paths used as
/// herdr cwd args and agent-mail keys must ROUND-TRIP — lossy conversion
/// would silently corrupt them. The wire protocols are JSON (UTF-8), so a
/// non-UTF-8 path is a clear error, never a panic and never silent
/// corruption.
pub(crate) fn utf8_path(p: &Path, what: &str) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{what} is not valid UTF-8: {p:?}"))
}

/// `project_dir` as a UTF-8 string (193.1): the project path is a real path
/// (herdr cwd, agent-mail key) — propagate non-UTF-8 as an error.
fn project_dir_str(cfg: &Config, project: &str) -> Result<String> {
    utf8_path(
        &project_dir(cfg, project),
        &format!("project path for {project:?}"),
    )
}

pub(crate) fn swarm_label(project: &str) -> String {
    let base = project.rsplit('/').next().unwrap_or(project).to_string();
    format!("flywheel-{base}")
}

/// bd-vik C: normalize a user-supplied workspace label to a slug —
/// lowercase ASCII alphanumerics + hyphens, runs collapsed, trimmed. Pure +
/// unit-tested. Returns (slug, changed) so the caller can warn.
fn slugify_label(label: &str) -> (String, bool) {
    let mut slug = String::with_capacity(label.len());
    let mut last_dash = false;
    for ch in label.chars() {
        let ok = ch.is_ascii_alphanumeric();
        if ok {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    let changed = slug != label;
    (slug, changed)
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
/// Labels that resolve `project` for status/send/kill: default swarm
/// label, spawn-meta custom label, the raw token, and its slug.
/// Captains pass `--label flywheel-omp-repro` then `status flywheel-omp-repro`
/// — that must hit, not only `flywheel-<dir>`.
fn workspace_label_aliases(project: &str, spawn_meta: Option<&str>) -> Vec<String> {
    let mut out = vec![swarm_label(project)];
    if let Some(meta) = spawn_meta
        && !out.iter().any(|l| l == meta)
    {
        out.push(meta.to_string());
    }
    if !out.iter().any(|l| l == project) {
        out.push(project.to_string());
    }
    let (slug, _) = slugify_label(project);
    if !slug.is_empty() && !out.iter().any(|l| l == &slug) {
        out.push(slug);
    }
    out
}

fn find_workspace(c: &mut Client, project: &str) -> Result<String> {
    let aliases = workspace_label_aliases(project, state::load_spawn_meta(project).as_deref());
    let matches: Vec<herdr_shim::WorkspaceInfo> = c
        .workspace_list()?
        .into_iter()
        .filter(|ws| aliases.iter().any(|a| a == &ws.label))
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
    let mut in_ws: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    if in_ws.is_empty() {
        bail!("no agents in workspace {ws_id} — is the swarm spawned?");
    }
    // bd-a2bw: nameless omp panes still resolve via the spawn registry.
    overlay_registry_names(&mut in_ws, &state::load_spawn_registry(project));
    let avail: Vec<String> = in_ws
        .iter()
        .map(|a| a.name.clone().unwrap_or_default())
        .collect();
    let want: Vec<WantToken> = if to == "all" {
        avail
            .iter()
            .cloned()
            .map(|a| WantToken {
                name: a,
                broaden: false,
                short: None,
                ordinal: 1,
            })
            .collect()
    } else {
        to.split(',').map(|s| to_want_token(project, s)).collect()
    };
    // bd-4pxy: name-based resolution is canonical; a short with no name hit
    // falls back to the ordinal-th agent of that kind.
    let idx = match_indices(&in_ws, &want);
    if idx.is_empty() {
        bail!(
            "no agents match --to {to} in {project} (available: {})",
            avail.join(", ")
        );
    }
    let matched: Vec<AgentInfo> = idx.iter().map(|&i| in_ws[i].clone()).collect();
    Ok(matched)
}

/// Trait-object overloads of `find_workspace` / `resolve_agents` for the
/// durable-loop leaf handlers (`__poll` / `__send` / `__mail-blocked`), which
/// construct their backend via the factory (`backend::multiplexer_backend`)
/// per ADR-0002 §5. The one-shot CLI commands keep the `&mut Client` overloads
/// (they're herdr-specific and don't need backend-agnosticism yet — migrating
/// them is churn with no payoff until superlogical ships). The two overloads
/// share the helper bodies via macros-free duplication (small, narrow).
fn find_workspace_dyn(backend: &dyn backend::MultiplexerBackend, project: &str) -> Result<String> {
    let aliases = workspace_label_aliases(project, state::load_spawn_meta(project).as_deref());
    let matches: Vec<herdr_shim::WorkspaceInfo> = backend
        .workspace_list()?
        .into_iter()
        .filter(|ws| aliases.iter().any(|a| a == &ws.label))
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
    let mut in_ws: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    if in_ws.is_empty() {
        bail!("no agents in workspace {ws_id} — is the swarm spawned?");
    }
    // bd-a2bw: nameless omp panes still resolve via the spawn registry.
    overlay_registry_names(&mut in_ws, &state::load_spawn_registry(project));
    let avail: Vec<String> = in_ws
        .iter()
        .map(|a| a.name.clone().unwrap_or_default())
        .collect();
    let want: Vec<WantToken> = if to == "all" {
        avail
            .iter()
            .cloned()
            .map(|a| WantToken {
                name: a,
                broaden: false,
                short: None,
                ordinal: 1,
            })
            .collect()
    } else {
        to.split(',').map(|s| to_want_token(project, s)).collect()
    };
    // bd-4pxy: name-based resolution is canonical; a short with no name hit
    // falls back to the ordinal-th agent of that kind.
    let idx = match_indices(&in_ws, &want);
    if idx.is_empty() {
        bail!(
            "no agents match --to {to} in {project} (available: {})",
            avail.join(", ")
        );
    }
    let matched: Vec<AgentInfo> = idx.iter().map(|&i| in_ws[i].clone()).collect();
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
/// A normalized `--to` token: the concrete agent name to match against, plus
/// whether the token was a BARE kind short (`oc`/`pi`/`gk`) that may broaden
/// to all numbered agents of that kind. Concrete names (`flywheel-<base>-oc`,
/// `flywheel-<base>-oc2`), numbered shorts (`oc2`), pane ids, and mail
/// identities match EXACTLY — a full name never silently drags in oc2/oc3.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WantToken {
    name: String,
    broaden: bool,
    /// The kind short when the token WAS a short (`omp1` → `omp`): the
    /// kind-based fallback for agents whose NAME diverges from their kind
    /// (bd-4pxy — an omp agent named `flywheel-<base>-coder`). `None` for
    /// concrete names, pane ids, and mail identities.
    short: Option<String>,
    /// 1-based ordinal of the kind short (`omp1` → 1, `omp2` → 2).
    ordinal: u32,
}

fn to_want_token(project: &str, token: &str) -> WantToken {
    let t = token.trim();
    if t.is_empty() || t.starts_with("flywheel-") || t.contains(':') {
        WantToken {
            name: t.to_string(),
            broaden: false,
            short: None,
            ordinal: 1,
        }
    } else if let Some((kind, ordinal)) = split_short_ordinal(t) {
        // bd-eign.14: a numbered short resolves to THAT ordinal's agent —
        // `oc1` → the first oc (`flywheel-<base>-oc`, which is NOT
        // digit-suffixed), `oc2` → `-oc2`. Exact match, no broadening.
        WantToken {
            name: agent_name_n(project, kind, ordinal),
            broaden: false,
            short: Some(kind.to_string()),
            ordinal,
        }
    } else {
        let broaden = matches!(t, "oc" | "pi" | "gk") && t.chars().all(|c| c.is_ascii_lowercase());
        WantToken {
            name: agent_name(project, t),
            broaden,
            short: Some(t.to_string()),
            ordinal: 1,
        }
    }
}

/// Split a numbered short (`oc1`, `pi2`, `gk3`) into kind + ordinal. The
/// spawn naming scheme (`agent_name_n`) keeps the FIRST agent of a kind bare
/// (`flywheel-<base>-oc`) and digit-suffixes later ones (`-oc2`), so the
/// ordinal must be applied to the kind, not appended to it (bd-eign.14).
fn split_short_ordinal(t: &str) -> Option<(&str, u32)> {
    let first_digit = t.find(|c: char| c.is_ascii_digit())?;
    let (kind, digits) = t.split_at(first_digit);
    if kind.is_empty() || digits.is_empty() || !kind.chars().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    digits.parse().ok().map(|n| (kind, n))
}

/// Exact match, or — for BARE kind shorts only — a digit-suffixed ordinal of
/// it (`oc` → oc, oc2, oc3). A concrete want (`flywheel-<base>-oc`) matches
/// exactly one agent; steering a single crew member no longer drags in its
/// numbered siblings.
fn name_matches_want(name: &str, want: &WantToken) -> bool {
    name == want.name
        || (want.broaden
            && name.starts_with(&want.name)
            && name[want.name.len()..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false))
}

/// The `ordinal`-th (1-based) agent of `kind` in list order — the kind
/// fallback for `--to` resolution (bd-4pxy): a short's NAME target may not
/// exist (an omp agent named `flywheel-<base>-coder`), but the herdr kind
/// is stable. `None` when fewer than `ordinal` agents run that kind.
fn kind_nth_index(agents: &[AgentInfo], kind: &str, ordinal: u32) -> Option<usize> {
    agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a.agent.as_deref() == Some(kind))
        .nth(ordinal.saturating_sub(1) as usize)
        .map(|(i, _)| i)
}

/// Indices of agents matched by the `--to` wants (bd-4pxy): name-based
/// resolution is canonical (exact, numbered, or broadened); when a short's
/// name target matches NOTHING, fall back to a pane-id exact match
/// (bd-a2bw — `--to w70:p3` must hit a nameless omp pane), then the
/// ordinal-th agent OF THAT KIND. Order preserved, deduped. Pure — shared
/// by the CLI and the backend-factory resolution paths.
fn match_indices(agents: &[AgentInfo], wants: &[WantToken]) -> Vec<usize> {
    let mut out = vec![false; agents.len()];
    for want in wants {
        let name_hits: Vec<usize> = agents
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                a.name
                    .as_deref()
                    .map(|n| name_matches_want(n, want))
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        if !name_hits.is_empty() {
            for i in name_hits {
                out[i] = true;
            }
        } else if let Some(i) = agents.iter().position(|a| a.pane_id == want.name) {
            // bd-a2bw: pane-id tokens never sit in `a.name`. Match the pane
            // even when herdr reports name=null (omp profile splits).
            out[i] = true;
        } else if let Some(short) = &want.short
            && let Some(i) = kind_nth_index(agents, short, want.ordinal)
        {
            out[i] = true;
        }
    }
    out.iter()
        .enumerate()
        .filter(|(_, hit)| **hit)
        .map(|(i, _)| i)
        .collect()
}

fn agent_target(a: &AgentInfo) -> &str {
    a.name.as_deref().unwrap_or(a.pane_id.as_str())
}

/// bd-a2bw / ADR-0018: herdr omp panes drop names (bd-w17). Fill null
/// names from the spawn registry so `--to coder3` matches after overlay
/// even though herdr kind stays `omp`.
fn overlay_registry_names(agents: &mut [AgentInfo], entries: &[state::SpawnEntry]) {
    for a in agents.iter_mut() {
        if a.name.is_some() {
            continue;
        }
        if let Some(e) = entries.iter().find(|e| e.pane_id == a.pane_id) {
            a.name = Some(e.name.clone());
        }
    }
}

/// bd-a2bw: resolve a send/rebind token to a spawn-registry row. Accepts
/// the herdr name, a pane id, or a numbered short (`coder3` →
/// `flywheel-<base>-coder3`).
fn registry_entry_for_token<'a>(
    entries: &'a [state::SpawnEntry],
    project: &str,
    token: &str,
) -> Option<&'a state::SpawnEntry> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    entries
        .iter()
        .find(|e| e.name == t || e.pane_id == t)
        .or_else(|| {
            let want = to_want_token(project, t);
            entries.iter().find(|e| e.name == want.name)
        })
}

/// bd-a2bw: herdr `launch_pending` is stale when the pane is already
/// input-ready (omp at `π >` with a session, status idle/working/blocked/
/// done). Spawn/send must not treat that as still-booting.
fn is_still_booting(a: &AgentInfo) -> bool {
    if !a.launch_pending {
        return false;
    }
    if a.interactive_ready
        && matches!(
            a.agent_status,
            AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Done
        )
    {
        return false;
    }
    true
}

/// Log an event with a compact payload.
fn log(project: &str, kind: EventKind, data: Value) {
    if let Err(e) = log_event(project, kind, data) {
        eprintln!("flywheel: cannot write event log: {e:#}");
    }
}

// ---- main dispatch ----

/// Rewrite the legacy bare form `flywheel loop <project> [flags]` into the
/// canonical `flywheel loop run <project> [flags]` before clap parsing.
/// `run` is the default subcommand; any first token after `loop` that is not
/// a known subcommand name is treated as the project. Pure + unit-tested.
fn rewrite_legacy_loop_args(args: &[String]) -> Vec<String> {
    const SUBCOMMANDS: [&str; 5] = ["run", "status", "list", "prune", "doctor"];
    if args.len() >= 2
        && args[1] == "loop"
        // A flag (--help, -h, global options) keeps the parent context;
        // anything else is the legacy positional project → default `run`.
        && let Some(next) = args.get(2)
        && !SUBCOMMANDS.contains(&next.as_str())
        && !next.starts_with('-')
    {
        let mut out = args.to_vec();
        out.insert(2, "run".to_string());
        return out;
    }
    args.to_vec()
}

fn main() {
    // clap parses from the raw argv; rewrite legacy `loop <project>` first.
    let raw: Vec<String> = std::env::args().collect();
    let rewritten = rewrite_legacy_loop_args(&raw);
    let cli = Cli::parse_from(&rewritten);

    if cli.robot_capabilities {
        print_json(&json!({
            "tool": "flywheel",
            "version": VERSION,
            "backend": format!("herdr (socket protocol v{})", herdr_shim::HDR_PROTOCOL),
            "commands": [
                "init spawn add list status decide rebind recover",
                "send interrupt view output watch zoom attach kill",
                "loop work assign collab xproj mail toron steer",
                "coalition debate plan-review profile squad model runs",
                "workflow cron recipes workflows pipeline template stage ensemble",
                "checkpoint timeline audit history changes resume analytics",
                "backend safety guards policy deps integration config upgrade tutorial serve preflight"
            ],
            "tracker": "br/bv (CLI, no MCP)",
            "coordination": "toron mail",
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
            no_verify,
            retire_orphans,
            peer_aware,
            no_peer_aware,
            peer_poll_interval_minutes,
            peer_poll_budget_minutes,
            peer_max_prompts,
            settle_secs,
            profile,
            count,
        }) => cmd_spawn(
            &cfg,
            project,
            counts,
            recipe.as_deref(),
            kinds,
            label.as_deref(),
            *no_focus,
            *no_verify,
            *retire_orphans,
            *peer_aware,
            *no_peer_aware,
            *peer_poll_interval_minutes,
            *peer_poll_budget_minutes,
            *peer_max_prompts,
            *settle_secs,
            profile.as_deref(),
            *count,
        ),
        Some(Commands::Add {
            project,
            counts,
            kinds,
        }) => cmd_add(&cfg, project, counts, kinds),
        Some(Commands::List { all }) => cmd_list(&cfg, *all),
        Some(Commands::Status { project }) => cmd_status(&cfg, project.as_deref()),
        Some(Commands::Decide {
            target,
            ready_beads,
            profiled_panes,
            launch_pending,
            contention,
            json,
        }) => cmd_decide(
            target,
            *ready_beads,
            *profiled_panes,
            *launch_pending,
            *contention,
            *json,
        ),
        Some(Commands::Rebind { project }) => cmd_rebind(&cfg, project),
        Some(Commands::Recover { project }) => cmd_recover(&cfg, project),
        Some(Commands::Send {
            project,
            msg,
            to,
            stage,
            wait,
            timeout_ms,
            force,
            respawn_on_stall,
        }) => cmd_send(
            &cfg,
            project,
            msg.as_deref(),
            to,
            stage.as_deref(),
            *wait,
            *timeout_ms,
            *force,
            *respawn_on_stall,
        ),
        Some(Commands::Interrupt {
            project,
            to,
            keys,
            force,
        }) => cmd_interrupt(&cfg, project, to, keys, *force),
        Some(Commands::View {
            project,
            to,
            lines,
            source,
        }) => cmd_view(&cfg, project, to, *lines, source),
        Some(Commands::Output {
            project,
            role,
            session,
            limit,
        }) => cmd_output(&cfg, project, role.as_deref(), session.as_deref(), *limit),
        Some(Commands::Watch {
            project,
            to,
            until,
            timeout_ms,
        }) => cmd_watch(&cfg, project, to, until.as_deref(), *timeout_ms),
        Some(Commands::Loop { cmd }) => match cmd {
            LoopCmd::Run {
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
                engine,
                force,
            } => {
                let pf_mode = if *work {
                    preflight::Mode::loop_work()
                } else {
                    preflight::Mode::loop_queue()
                };
                preflight_gate(&cfg, cli.skip_preflight, pf_mode, Some(project.as_str())).and_then(
                    |_| {
                        cmd_loop(
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
                            *engine,
                            *force,
                        )
                    },
                )
            }
            LoopCmd::Status { project } => cmd_loop_status(&cfg, project),
            LoopCmd::List => cmd_loop_list(),
            LoopCmd::Prune => cmd_loop_prune(),
            LoopCmd::Doctor { fix, json } => cmd_loop_doctor(&cfg, *fix, *json),
        },
        Some(Commands::Zoom { project, to, off }) => cmd_zoom(&cfg, project, to, *off),
        Some(Commands::Attach { project, to }) => cmd_attach(&cfg, project, to),
        Some(Commands::Kill {
            project,
            force,
            force_working,
        }) => cmd_kill(&cfg, project, *force, *force_working),
        Some(Commands::Work { cmd }) => cmd_work(&cfg, cmd),
        Some(Commands::Assign {
            project,
            to,
            beads,
            msg,
        }) => cmd_assign(&cfg, project, to, beads, msg.as_deref()),
        Some(Commands::Mail { cmd }) => cmd_mail(&cfg, cmd),
        Some(Commands::Collab {
            project,
            to,
            subject,
            body,
            timeout_secs,
            as_agent,
        }) => cmd_collab(
            &cfg,
            project,
            to,
            subject,
            body,
            *timeout_secs,
            as_agent.as_deref(),
        ),
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
        Some(Commands::Coalition { cmd }) => cmd_coalition(cmd),
        Some(Commands::Runs { cmd }) => cmd_runs(&cfg, cmd),
        Some(Commands::Steer {
            objective,
            direct,
            crew,
            project,
            reuse,
            retain,
            retire_orphans,
            json,
        }) => preflight_gate(
            &cfg,
            cli.skip_preflight,
            preflight::Mode::steer(),
            project.as_deref(),
        )
        .and_then(|_| {
            cmd_steer(
                &cfg,
                objective,
                *direct,
                crew.as_deref(),
                project.as_deref(),
                reuse.as_deref(),
                *retain,
                *retire_orphans,
                *json,
            )
        }),
        Some(Commands::Workflow { cmd }) => {
            let needs_pf =
                !cli.skip_preflight && matches!(cmd, WorkflowCmd::Run { execute: true, .. });
            let pf = if needs_pf {
                let project = match &cmd {
                    WorkflowCmd::Run { project, .. } => project.as_deref(),
                    _ => None,
                };
                preflight_gate(&cfg, false, preflight::Mode::workflow(), project)
            } else {
                Ok(())
            };
            pf.and_then(|_| cmd_workflow(&cfg, cmd))
        }
        Some(Commands::Model { cmd }) => cmd_models(cmd),
        Some(Commands::Cron { cmd }) => cmd_cron(&cfg, cmd),
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
        Some(Commands::Preflight {
            mode,
            project,
            work,
            json,
        }) => cmd_preflight(&cfg, mode, project.as_deref(), *work, *json),
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
            profile,
        }) => cmd_internal_send(
            &cfg,
            project,
            name,
            text,
            SendOptions {
                confirm_ms: *confirm,
                invocation: invocation.as_deref(),
                iteration: *iteration,
                profile: profile.as_deref(),
            },
        ),
        Some(Commands::__MailBlocked {
            project,
            name,
            tier,
        }) => cmd_internal_mail_blocked(&cfg, project, name, *tier),
        Some(Commands::__MailNotReady {
            project,
            bead_id,
            title,
        }) => cmd_internal_mail_not_ready(&cfg, project, bead_id, title),
        Some(Commands::__Diagnose {
            project,
            name,
            reason,
        }) => cmd_internal_diagnose(&cfg, project, name, reason),
        Some(Commands::__XprojInbox { project, limit }) => {
            cmd_internal_xproj_inbox(&cfg, project, *limit)
        }
        Some(Commands::__MailDigest { project, agent }) => {
            cmd_internal_mail_digest(&cfg, project, agent)
        }
        Some(Commands::__CheckReply {
            project,
            name,
            thread,
        }) => cmd_internal_check_reply(&cfg, project, name, thread),
        Some(Commands::__ReplySettled {
            project,
            name,
            thread,
        }) => match state::remove_expected_reply(project, name, thread) {
            Ok(()) => {
                print_json(&json!({ "ok": true, "removed": { "agent": name, "thread": thread } }));
                Ok(())
            }
            Err(e) => Err(e),
        },
        Some(Commands::__ExpectedReplies { project }) => {
            print_json(&json!({ "entries": state::load_expected_replies(project) }));
            Ok(())
        }
        Some(Commands::__RemindReply {
            project,
            name,
            thread,
            reminder_count,
            invocation,
            iteration,
        }) => cmd_internal_remind_reply(
            project,
            name,
            thread,
            *reminder_count,
            invocation.as_deref(),
            *iteration,
        ),
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
        Some(Commands::__ReleaseBeadLocks { project, bead_id }) => {
            cmd_internal_release_bead_locks(&cfg, project, bead_id)
        }
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
        Some(Commands::__ReclaimStranded { project, live }) => {
            cmd_internal_reclaim_stranded(&cfg, project, live)
        }
        Some(Commands::__Kind { project, name }) => cmd_internal_kind(&cfg, project, name),
        Some(Commands::__Profile { project, name }) => cmd_internal_profile(&cfg, project, name),
        Some(Commands::__Read { project, name }) => cmd_internal_read(&cfg, project, name),
        Some(Commands::__Respawn {
            project,
            name,
            kind,
            force,
        }) => cmd_internal_respawn(&cfg, project, name, kind, *force),
        Some(Commands::__RunHeadless {
            profile,
            prompt,
            model,
            harness,
            workflow,
            cron,
            json,
        }) => cmd_internal_run_headless(
            profile,
            prompt,
            model.as_deref(),
            harness.as_deref(),
            workflow.as_deref(),
            cron.as_deref(),
            *json,
        ),
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
        if let Some(n) = n
            && n > 0
        {
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

/// One spawn unit in [`cmd_spawn`]: a kind short (`oc`/`pi`/`gk`/kind name) or
/// a profile name, with its count. Profile items carry the resolved harness
/// (`profile.harness`) and the profile name; plain items resolve the kind via
/// `Config::resolve_kind` at spawn time (bd-eign.9/13).
struct SpawnItem {
    /// name base: the kind short for plain items, the profile name for
    /// profiled items (`flywheel-<base>-<short>-<n>`).
    short: String,
    count: u32,
    /// resolved harness for profiled items; `None` → resolve_kind(short).
    kind: Option<String>,
    /// the profile name when this item is profile-driven.
    profile: Option<String>,
}

/// Merge the recipe/count/kind plan with an optional profile-driven plan
/// (bd-eign.13: `flywheel spawn <proj> --profile coder --count 2`). Pure —
/// the catalog lookup happens at the call site, tests inject a profile here.
fn spawn_items_from_plan(
    plan: Vec<(String, u32)>,
    profile: Option<(&str, &profile::Profile, u32)>,
) -> Vec<SpawnItem> {
    let mut items: Vec<SpawnItem> = plan
        .into_iter()
        .map(|(short, count)| SpawnItem {
            short,
            count,
            kind: None,
            profile: None,
        })
        .collect();
    if let Some((name, p, count)) = profile {
        items.push(SpawnItem {
            short: name.to_string(),
            count: count.max(1),
            kind: Some(p.harness.clone()),
            profile: Some(name.to_string()),
        });
    }
    items
}

/// Model + role-model launch args for a profiled spawn (bd-eign.9): the
/// per-harness `-m <model>` / `--model=<model>` flag (headless.rs) plus the
/// omp role flags (`--smol`/`--slow`/`--plan`, bd-gsvn.2). Order matters —
/// flags precede any launch text (peer template) so a one-shot-capable
/// harness (opencode) does not swallow the flags as a prompt.
pub(crate) fn profile_launch_args(harness: &str, p: &profile::Profile) -> Vec<String> {
    let registry = models::ModelRegistry::load().unwrap_or_default();
    let model = if p.model.is_empty() {
        String::new()
    } else {
        registry.resolve(&p.model, harness)
    };
    let mut args = headless::model_args(
        harness,
        &model,
        (!p.effort.is_empty()).then_some(p.effort.as_str()),
    );
    args.extend(headless::role_model_args(
        harness,
        &registry.resolve(&p.smol, harness),
        &registry.resolve(&p.slow, harness),
        &registry.resolve(&p.plan, harness),
    ));
    args
}
/// clap command signature — params map 1:1 to CLI flags (ADR-0002 CLI surface).
#[allow(clippy::too_many_arguments)]
/// Render the peer-aware self-poll template (ADR-0001 §9.3) with all
/// placeholders substituted. Pure + deterministic so the injected prompt is
/// free of literal `{placeholder}` text (bd-pattern-peer-triggered).
///
/// S8.3: `mail_identity` (the registered canonical name, resolved BEFORE
/// the agent starts) is injected as "your mail identity is X" so the agent
/// boots knowing the exact name its peers/assigns address it by.
fn peer_prompt_template(
    tpl: &str,
    name: &str,
    mail_identity: &str,
    poll_interval_minutes: u32,
    poll_budget_minutes: u32,
    max_peer_prompts: u32,
) -> String {
    tpl.replace("{name}", name)
        .replace("{mail_identity}", mail_identity)
        .replace("{task}", "await the swarm loop's queue or help peers")
        .replace(
            "{poll_interval_minutes}",
            &poll_interval_minutes.to_string(),
        )
        .replace("{poll_budget_minutes}", &poll_budget_minutes.to_string())
        .replace("{max_peer_prompts}", &max_peer_prompts.to_string())
}

/// clap command signature — params map 1:1 to CLI flags (ADR-0002 CLI surface).
#[allow(clippy::too_many_arguments)]
/// bd-fw-stale-workspace-panes-hgp + bd-627z.3: pre-existing herdr
/// workspaces that share this project's label or host its agents, split by
/// origin. Flywheel-owned workspaces (default swarm label `flywheel-<base>`,
/// a recorded custom spawn label, or hosting agents named `flywheel-<base>-*`)
/// are the stale-pane race; a foreign workspace (captain/omp session, another
/// tool) that merely shares the project-base label is NOT — it must not block
/// a spawn (warn + proceed, never closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceOrigin {
    Owned,
    Foreign,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceSplit {
    pub owned: Vec<herdr_shim::WorkspaceInfo>,
    pub foreign: Vec<herdr_shim::WorkspaceInfo>,
}

/// bd-627z.3: classify one workspace by origin. `None` = unrelated (skip).
/// Owned wins over Foreign when both signals collide (a flywheel marker on a
/// base-labeled workspace is still a flywheel workspace).
fn classify_workspace_origin(
    project: &str,
    ws: &herdr_shim::WorkspaceInfo,
    agents: &[herdr_shim::AgentInfo],
    spawn_label: Option<&str>,
) -> Option<WorkspaceOrigin> {
    let base = project_base(project);
    let label = swarm_label(project);
    let prefix = format!("flywheel-{base}-");
    let hosts_agents = agents.iter().any(|a| {
        a.workspace_id == ws.workspace_id
            && a.name
                .as_deref()
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
    });
    if ws.label == label || hosts_agents || spawn_label == Some(ws.label.as_str()) {
        Some(WorkspaceOrigin::Owned)
    } else if ws.label == base {
        Some(WorkspaceOrigin::Foreign)
    } else {
        None
    }
}

/// bd-627z.3: pure partition of the workspace list into owned/foreign.
fn partition_workspaces(
    project: &str,
    ws_list: Vec<herdr_shim::WorkspaceInfo>,
    agents: &[herdr_shim::AgentInfo],
    spawn_label: Option<&str>,
) -> WorkspaceSplit {
    let mut split = WorkspaceSplit::default();
    for ws in ws_list {
        match classify_workspace_origin(project, &ws, agents, spawn_label) {
            Some(WorkspaceOrigin::Owned) => split.owned.push(ws),
            Some(WorkspaceOrigin::Foreign) => split.foreign.push(ws),
            None => {}
        }
    }
    split
}

pub(crate) fn project_workspaces(c: &mut Client, project: &str) -> Result<WorkspaceSplit> {
    let ws_list = c.workspace_list()?;
    let agents = c.agent_list().unwrap_or_default();
    Ok(partition_workspaces(
        project,
        ws_list,
        &agents,
        state::load_spawn_meta(project).as_deref(),
    ))
}

/// bd-bug-add-foreign-workspace-m7dg: the add path's target workspace. Same
/// partition as spawn: owned swarm workspace required, foreign (captain/omp
/// session sharing the base label) never targeted — the old find_workspace
/// matched the base label and added panes into the captain's own workspace
/// (w37, label toron) instead of the flywheel-owned swarm (label
/// flywheel-toron). Refuses when no owned workspace exists (spawn first).
fn add_target_from_split(
    split: &WorkspaceSplit,
    agents: &[herdr_shim::AgentInfo],
    project: &str,
) -> Result<String> {
    if split.owned.is_empty() {
        bail!("no flywheel-owned swarm for {project} (run: flywheel spawn {project})")
    }
    // prefer the owned workspace hosting managed agents; first owned otherwise
    // (resolve_workspace_id picks the agent-hosting workspace among many).
    let prefix = format!("flywheel-{}-", project_base(project));
    Ok(herdr_shim::resolve_workspace_id(
        &split.owned,
        agents,
        &prefix,
    ))
}

fn add_target_workspace(c: &mut Client, project: &str) -> Result<String> {
    let split = project_workspaces(c, project)?;
    if !split.foreign.is_empty() {
        let detail: Vec<String> = split
            .foreign
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: warn: {} foreign workspace(s) share the {project} label but are not flywheel-owned — leaving them alone: {}",
            split.foreign.len(),
            detail.join(", ")
        );
    }
    let agents = c.agent_list()?;
    add_target_from_split(&split, &agents, project)
}

#[cfg(test)]
mod spawn_profile_plan_tests {
    use super::*;
    use crate::config::lock_env;

    fn profile(harness: &str, model: &str) -> profile::Profile {
        profile::Profile {
            harness: harness.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_plan_passes_through() {
        let plan = vec![("oc".to_string(), 2), ("pi".to_string(), 1)];
        let items = spawn_items_from_plan(plan, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].short, "oc");
        assert_eq!(items[0].count, 2);
        assert!(items[0].kind.is_none());
        assert!(items[0].profile.is_none());
        assert!(items[1].profile.is_none());
    }

    #[test]
    fn profile_item_carries_harness_and_name_base() {
        let items = spawn_items_from_plan(
            vec![],
            Some(("coder", &profile("opencode", "deepseek-v4-flash"), 2)),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].short, "coder");
        assert_eq!(items[0].count, 2);
        assert_eq!(items[0].kind.as_deref(), Some("opencode"));
        assert_eq!(items[0].profile.as_deref(), Some("coder"));
    }

    #[test]
    fn profile_count_floor_is_one() {
        let items = spawn_items_from_plan(vec![], Some(("coder", &profile("opencode", "m"), 0)));
        assert_eq!(items[0].count, 1);
    }

    #[test]
    fn profile_merges_with_plain_for_mixed_crews() {
        let items = spawn_items_from_plan(
            vec![("oc".to_string(), 1)],
            Some(("reviewer", &profile("pi", "smol"), 1)),
        );
        assert_eq!(items.len(), 2);
        assert!(items[0].profile.is_none());
        assert_eq!(items[1].profile.as_deref(), Some("reviewer"));
        assert_eq!(items[1].kind.as_deref(), Some("pi"));
    }

    /// Registry provider must prefix the spawn flag (coder → opencode-go,
    /// not a bare id that omp then binds to provider `deepseek`).
    #[test]
    fn launch_args_resolve_registry_provider() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        std::fs::write(
            cfg_home.join("flywheel/models.toml"),
            "[models.deepseek-v4-flash]\nprovider = \"opencode-go\"\n",
        )
        .expect("models.toml");
        let p = profile("omp", "deepseek-v4-flash");
        let args = profile_launch_args("omp", &p);
        assert_eq!(
            args,
            vec!["--model=opencode-go/deepseek-v4-flash"],
            "spawn must inject registry provider, not bare id: {args:?}"
        );
    }

    #[test]
    fn launch_args_follow_harness_forms() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let p = profile("opencode", "deepseek-v4-flash");
        assert_eq!(
            profile_launch_args("opencode", &p),
            vec!["-m", "deepseek-v4-flash"]
        );
        let pi = profile("pi", "smol-model");
        assert_eq!(
            profile_launch_args("pi", &pi),
            vec!["--model", "smol-model"]
        );
        let mut omp = profile("omp", "glm-5.2");
        omp.effort = "max".into();
        omp.smol = "glm-4.5-air".into();
        assert_eq!(
            profile_launch_args("omp", &omp),
            vec!["--model=glm-5.2", "--thinking=max", "--smol=glm-4.5-air"]
        );
        let mut unknown = profile("hermes", "x");
        unknown.effort = "max".into();
        assert!(profile_launch_args("hermes", &unknown).is_empty());
    }

    #[test]
    fn launch_args_precede_peer_text() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let p = profile("opencode", "deepseek-v4-flash");
        let mut args = profile_launch_args("opencode", &p);
        args.extend(vec!["peer template".to_string()]);
        assert_eq!(args, vec!["-m", "deepseek-v4-flash", "peer template"]);
    }

    #[test]
    fn skill_prefix_built_once_for_fresh_profiled_agent() {
        let entry = state::SpawnEntry::profiled(
            "w1:p1",
            "flywheel-flywheel-coder",
            "coder",
            "opencode",
            None,
        );
        let skills = vec!["code-quality".to_string(), "tdd".to_string()];
        let p =
            skill_injection_prefix(&entry, &skills).expect("fresh profiled agent gets the prefix");
        assert_eq!(
            p,
            "Read skill://code-quality skill://tdd before starting.\n\n"
        );
        let mut injected = entry;
        injected.skills_injected = true;
        assert!(skill_injection_prefix(&injected, &skills).is_none());
    }

    #[test]
    fn skill_prefix_skips_legacy_and_skillless() {
        let legacy = state::SpawnEntry::legacy("w1:p1", "flywheel-flywheel-oc");
        assert!(skill_injection_prefix(&legacy, &["x".to_string()]).is_none());
        let profiled = state::SpawnEntry::profiled(
            "w1:p1",
            "flywheel-flywheel-coder",
            "coder",
            "opencode",
            None,
        );
        assert!(skill_injection_prefix(&profiled, &[]).is_none());
    }
}

#[cfg(test)]
mod profile_run_spawn_tests {
    use super::*;
    use crate::config::lock_env;

    fn profile(harness: &str, model: &str) -> profile::Profile {
        profile::Profile {
            harness: harness.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    /// Hermetic cfg: temp workspace root; the state dir must be isolated via
    /// XDG_STATE_HOME (state::state_dir honors it — config.rs 193.2), and
    /// the env lock serializes against other env-mutating tests.
    fn hermetic() -> (Config, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_home = dir.path().join("state");
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
        let cfg = Config {
            workspace_root: Some(dir.path().join("ws").to_string_lossy().into_owned()),
            ..Default::default()
        };
        (cfg, dir)
    }

    #[test]
    fn profile_run_launch_args_non_empty_for_known_profile() {
        // bd-l2ky: `profile run coder` must launch with the SAME args as
        // `spawn --profile coder` — model + effort, never an empty arg list.
        let mut coder = profile("omp", "glm-5.2");
        coder.effort = "max".into();
        let args = profile_launch_args("omp", &coder);
        assert!(
            !args.is_empty(),
            "known profile must inject model/effort args"
        );
        assert!(
            args.iter().any(|a| a.contains("glm-5.2")),
            "model flag present: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.contains("max")),
            "effort flag present: {args:?}"
        );
    }

    /// bd-ude8: `resolved_spec_workspace` must return a PANE id — `profile
    /// run`/`squad spawn` hand it to `agent_start` as the pane, and herdr
    /// rejects a workspace id with `agent_pane_not_found`.
    #[test]
    fn resolved_spec_workspace_returns_pane_id() {
        use crate::backend::MultiplexerBackend;
        use crate::herdr_shim::{PaneInfo, WorkspaceInfo};
        use toron_core::backend::types::AgentStatus;

        struct MockMux {
            ws: Vec<WorkspaceInfo>,
            panes: Vec<PaneInfo>,
        }
        impl MultiplexerBackend for MockMux {
            fn name(&self) -> &'static str {
                "mock-mux"
            }
            fn workspace_list(
                &self,
            ) -> Result<Vec<WorkspaceInfo>, toron_core::backend::BackendError> {
                Ok(self.ws.clone())
            }
            fn workspace_create(
                &self,
                _label: Option<&str>,
            ) -> Result<WorkspaceInfo, toron_core::backend::BackendError> {
                let id = "ws-1".to_string();
                Ok(WorkspaceInfo {
                    workspace_id: id,
                    number: 1,
                    label: "mock".into(),
                    focused: false,
                    pane_count: 1,
                    tab_count: 1,
                    active_tab_id: "t1".into(),
                    agent_status: AgentStatus::Idle,
                    cwd: None,
                })
            }
            fn pane_list(
                &self,
                workspace_id: Option<&str>,
            ) -> Result<Vec<PaneInfo>, toron_core::backend::BackendError> {
                let ws = workspace_id.unwrap_or_default();
                Ok(self
                    .panes
                    .iter()
                    .filter(|p| p.workspace_id == ws)
                    .cloned()
                    .collect())
            }
        }
        fn pane(ws: &str, n: u32) -> PaneInfo {
            PaneInfo {
                pane_id: format!("{ws}:p{n}"),
                terminal_id: format!("term-{n}"),
                workspace_id: ws.into(),
                tab_id: format!("{ws}:t1"),
                focused: false,
                agent_status: AgentStatus::Idle,
                revision: 0,
                title: None,
                cwd: None,
                agent: None,
                agent_session: None,
                display_agent: None,
                foreground_cwd: None,
                label: None,
                scroll: None,
                state_labels: serde_json::Map::new(),
                terminal_title: None,
                terminal_title_stripped: None,
                tokens: serde_json::Map::new(),
            }
        }
        let _g = lock_env();
        let (cfg, _dir) = hermetic();

        // existing workspace -> first PANE of it, not the workspace id
        let mux = MockMux {
            ws: vec![WorkspaceInfo {
                workspace_id: "wA".into(),
                number: 1,
                label: "proj".into(),
                focused: false,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "wA:t1".into(),
                agent_status: AgentStatus::Idle,
                cwd: None,
            }],
            panes: vec![pane("wA", 1)],
        };
        let got = resolved_spec_workspace(&cfg, &mux, "omp").expect("resolves");
        assert_eq!(got, "wA:p1", "must return a pane id, not workspace id");

        // no workspace -> create one and return its root pane
        let mux2 = MockMux {
            ws: vec![],
            panes: vec![pane("ws-1", 1)],
        };
        let got2 = resolved_spec_workspace(&cfg, &mux2, "omp").expect("resolves");
        assert_eq!(got2, "ws-1:p1", "created workspace's root pane");
    }

    #[test]
    fn record_spawn_entry_records_profile_for_conditioning() {
        // bd-l2ky: after `profile run` / `squad spawn`, __send --profile and
        // the loop must resolve the agent's profile from the spawn registry.
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let entry = state::SpawnEntry::profiled(
            "w1:p1",
            "profile-coder",
            "coder",
            "omp",
            Some("loud-lion".into()),
        );
        record_spawn_entry(&cfg, "flywheel", entry);
        let key = project_dir_str(&cfg, "flywheel").expect("key");
        let registry = state::load_spawn_registry(&key);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].name, "profile-coder");
        assert_eq!(registry[0].profile.as_deref(), Some("coder"));
        assert_eq!(registry[0].harness.as_deref(), Some("omp"));
    }

    #[test]
    fn record_spawn_entry_preserves_others_and_replaces_same_name() {
        // bd-l2ky: the merge must not clobber unrelated agents (a squad
        // spawn appends per member); a re-spawn of the same name replaces
        // its own row only.
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        record_spawn_entry(
            &cfg,
            "flywheel",
            state::SpawnEntry::profiled("w1:p1", "profile-coder", "coder", "omp", None),
        );
        record_spawn_entry(
            &cfg,
            "flywheel",
            state::SpawnEntry::profiled("w1:p2", "profile-planner", "planner", "omp", None),
        );
        record_spawn_entry(
            &cfg,
            "flywheel",
            state::SpawnEntry::profiled("w1:p9", "profile-coder", "coder", "omp", None),
        );
        let key = project_dir_str(&cfg, "flywheel").expect("key");
        let registry = state::load_spawn_registry(&key);
        assert_eq!(registry.len(), 2, "coder row replaced, planner preserved");
        let coder = registry
            .iter()
            .find(|e| e.name == "profile-coder")
            .expect("coder row present");
        assert_eq!(
            coder.pane_id, "w1:p9",
            "re-spawn replaces the same-name row"
        );
        assert_eq!(coder.profile.as_deref(), Some("coder"));
        assert!(
            registry.iter().any(|e| e.name == "profile-planner"),
            "unrelated rows preserved"
        );
    }
}

#[cfg(test)]
mod internal_respawn_tests {
    use super::*;
    use crate::backend::MultiplexerBackend;
    use crate::config::lock_env;
    use crate::herdr_shim::{PaneInfo, WorkspaceInfo};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hermetic cfg: temp workspace root + temp XDG dirs. `XDG_CONFIG_HOME`
    /// isolates the profile catalog (profiles.toml), `XDG_STATE_HOME` the
    /// state dir (spawn registry).
    fn hermetic() -> (Config, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_home = dir.path().join("state");
        let config_home = dir.path().join("config");
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config_home) };
        std::fs::create_dir_all(config_home.join("flywheel")).expect("config dir");
        let cfg = Config {
            workspace_root: Some(dir.path().join("ws").to_string_lossy().into_owned()),
            ..Default::default()
        };
        (cfg, dir)
    }

    fn write_profiles_toml(dir: &tempfile::TempDir) {
        std::fs::write(
            dir.path().join("config/flywheel/profiles.toml"),
            "[profiles.omp-coder]\nharness = \"omp\"\nmodel = \"glm-5.2\"\neffort = \"max\"\nsmol = \"glm-4.5-air\"\n",
        )
        .expect("profiles.toml");
    }

    fn agent(name: &str, pane: &str, ws: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            agent: Some(name.to_string()),
            agent_status: status,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: pane.to_string(),
            revision: 0,
            state_change_seq: 0,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: ws.to_string(),
        }
    }

    fn pane(ws: &str, n: u32) -> PaneInfo {
        PaneInfo {
            pane_id: format!("{ws}:p{n}"),
            terminal_id: format!("term-{n}"),
            workspace_id: ws.into(),
            tab_id: format!("{ws}:t1"),
            focused: false,
            agent_status: AgentStatus::Idle,
            revision: 0,
            title: None,
            cwd: None,
            agent: None,
            agent_session: None,
            display_agent: None,
            foreground_cwd: None,
            label: None,
            scroll: None,
            state_labels: serde_json::Map::new(),
            terminal_title: None,
            terminal_title_stripped: None,
            tokens: serde_json::Map::new(),
        }
    }

    fn workspace(label: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: "wA".into(),
            number: 1,
            label: label.into(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: "wA:t1".into(),
            agent_status: AgentStatus::Idle,
            cwd: None,
        }
    }

    struct MockMux {
        ws_label: String,
        agents: Mutex<Vec<AgentInfo>>,
        started: Mutex<Vec<StartedAgent>>,
        splits: AtomicUsize,
        closes: AtomicUsize,
    }

    /// (name, kind, pane_id, args) captured from each `agent_start`.
    type StartedAgent = (String, String, String, Vec<String>);

    impl MockMux {
        fn new(ws_label: &str, agents: Vec<AgentInfo>) -> Self {
            Self {
                ws_label: ws_label.into(),
                agents: Mutex::new(agents),
                started: Mutex::new(Vec::new()),
                splits: AtomicUsize::new(0),
                closes: AtomicUsize::new(0),
            }
        }
    }

    impl MultiplexerBackend for MockMux {
        fn name(&self) -> &'static str {
            "mock-mux"
        }
        fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, toron_core::backend::BackendError> {
            Ok(vec![workspace(&self.ws_label)])
        }
        fn pane_list(
            &self,
            _workspace_id: Option<&str>,
        ) -> Result<Vec<PaneInfo>, toron_core::backend::BackendError> {
            Ok(self
                .agents
                .lock()
                .expect("mock agents lock")
                .iter()
                .map(|a| pane(&a.workspace_id, 1))
                .collect())
        }
        fn pane_split(
            &self,
            _parent_pane_id: &str,
            _direction: &str,
            _cwd: Option<&str>,
        ) -> Result<PaneInfo, toron_core::backend::BackendError> {
            self.splits.fetch_add(1, Ordering::SeqCst);
            Ok(pane("wA", 2))
        }
        fn pane_close(&self, _pane_id: &str) -> Result<(), toron_core::backend::BackendError> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn agent_list(&self) -> Result<Vec<AgentInfo>, toron_core::backend::BackendError> {
            Ok(self.agents.lock().expect("mock agents lock").clone())
        }
        fn agent_start(
            &self,
            name: &str,
            kind: &str,
            pane_id: &str,
            args: &[String],
        ) -> Result<AgentInfo, toron_core::backend::BackendError> {
            self.started.lock().expect("mock started lock").push((
                name.into(),
                kind.into(),
                pane_id.into(),
                args.to_vec(),
            ));
            let mut agents = self.agents.lock().expect("mock agents lock");
            agents.push(agent(name, pane_id, "wA", AgentStatus::Idle));
            Ok(agents.last().expect("just pushed").clone())
        }
        fn agent_rename(
            &self,
            _target: &str,
            _new_name: Option<&str>,
        ) -> Result<(), toron_core::backend::BackendError> {
            Ok(())
        }
    }

    /// bd-ude8: an omp respawn must re-inject the profile's model/effort
    /// launch args (never the empty `&[]` argv) AND restart in the SAME
    /// pane — `pane.close` on the old pane delivers a PTY stdin EOF that
    /// kills omp-in-herdr before it boots.
    #[test]
    fn omp_respawn_reinjects_args_and_stays_in_pane() {
        let _g = lock_env();
        let (cfg, dir) = hermetic();
        write_profiles_toml(&dir);
        record_spawn_entry(
            &cfg,
            "demo",
            state::SpawnEntry::profiled("wA:p1", "profile-coder", "omp-coder", "omp", None),
        );
        let mux = MockMux::new(
            "flywheel-demo",
            vec![agent("profile-coder", "wA:p1", "wA", AgentStatus::Idle)],
        );

        respawn_with_backend(&mux, &cfg, "demo", "profile-coder", "omp", true).expect("respawn ok");

        assert_eq!(
            mux.splits.load(Ordering::SeqCst),
            0,
            "omp respawn must NOT split a new pane"
        );
        assert_eq!(
            mux.closes.load(Ordering::SeqCst),
            0,
            "omp respawn must NOT pane.close (PTY stdin EOF kills omp)"
        );
        let started = mux.started.lock().expect("mock started lock");
        assert_eq!(started.len(), 1);
        let (name, kind, pane_id, args) = &started[0];
        assert_eq!(name, "profile-coder");
        assert_eq!(kind, "omp");
        assert_eq!(pane_id, "wA:p1", "omp restarts in the SAME pane");
        assert!(!args.is_empty(), "omp respawn argv must be non-empty");
        assert!(
            args.iter().any(|a| a.contains("glm-5.2")),
            "omp model flag present: {args:?}"
        );
    }

    /// bd-ude8: non-omp kinds keep the split+close cycle.
    #[test]
    fn non_omp_respawn_splits_and_closes_old_pane() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let mux = MockMux::new(
            "flywheel-demo",
            vec![agent("flywheel-demo-oc", "wA:p1", "wA", AgentStatus::Idle)],
        );

        respawn_with_backend(&mux, &cfg, "demo", "flywheel-demo-oc", "opencode", true)
            .expect("respawn ok");

        assert_eq!(
            mux.splits.load(Ordering::SeqCst),
            1,
            "non-omp respawn splits a fresh pane"
        );
        assert_eq!(
            mux.closes.load(Ordering::SeqCst),
            1,
            "non-omp respawn closes the old pane"
        );
        let started = mux.started.lock().expect("mock started lock");
        assert_eq!(started.len(), 1);
        assert_eq!(
            started[0].2, "wA:p2",
            "agent starts on the fresh split pane"
        );
        assert!(
            started[0].3.is_empty(),
            "unprofiled agent has no registry profile → unchanged empty argv"
        );
    }

    /// bd-ude8: respawn argv comes ONLY from the registry-pinned profile —
    /// legacy entries (no profile) and unknown names keep the empty argv.
    #[test]
    fn respawn_args_only_from_registry_pinned_profile() {
        let _g = lock_env();
        let (cfg, dir) = hermetic();
        write_profiles_toml(&dir);
        let key = project_dir_str(&cfg, "demo").expect("key");
        state::save_spawn_registry(
            &key,
            &[
                state::SpawnEntry::profiled("wA:p1", "profile-coder", "omp-coder", "omp", None),
                state::SpawnEntry::legacy("wA:p2", "flywheel-demo-oc"),
            ],
        )
        .expect("registry");

        let omp = respawn_launch_args(&cfg, "demo", "profile-coder", "omp");
        assert!(!omp.is_empty(), "profiled agent re-injects its launch args");
        assert!(
            omp.iter().any(|a| a.contains("glm-5.2")),
            "model flag: {omp:?}"
        );

        let legacy = respawn_launch_args(&cfg, "demo", "flywheel-demo-oc", "opencode");
        assert!(legacy.is_empty(), "legacy entry (no profile) → empty argv");

        let unknown = respawn_launch_args(&cfg, "demo", "no-such-agent", "pi");
        assert!(unknown.is_empty(), "unknown agent → empty argv");
    }
}

#[cfg(test)]
mod profile_workflow_invoke_tests {
    use super::*;

    /// bd-6zsl: `profile run --workflow` must ATTEMPT a real run — the
    /// invoke command is a pane-mode Run with --execute against the spawned
    /// agent's workspace (--reuse, never reaped), not a "not yet wired"
    /// message.
    #[test]
    fn invoke_attempts_pane_run_in_spawned_workspace() {
        let cmd =
            profile_workflow_invoke("weekly-review", Some("{\"goal\":\"x\"}"), "ws-1", "toron");
        let WorkflowCmd::Run {
            name,
            inputs,
            execute,
            mode,
            project,
            reuse,
            ..
        } = cmd
        else {
            panic!("expected a Run command");
        };
        assert_eq!(name, "weekly-review");
        assert_eq!(inputs.as_deref(), Some("{\"goal\":\"x\"}"));
        assert!(execute, "invoke must execute, not record");
        assert_eq!(mode, "pane");
        assert_eq!(project.as_deref(), Some("toron"));
        assert_eq!(reuse.as_deref(), Some("ws-1"));
    }

    /// No inputs → the workflow runs with the engine default ({}).
    #[test]
    fn invoke_without_inputs_runs_default() {
        let cmd = profile_workflow_invoke("plan", None, "ws-1", "toron");
        let WorkflowCmd::Run {
            inputs, execute, ..
        } = cmd
        else {
            panic!("expected a Run command");
        };
        assert!(inputs.is_none());
        assert!(execute);
    }
}

#[cfg(test)]
mod steer_safety_tests {
    use super::*;

    #[test]
    fn workspace_label_aliases_include_raw_label_and_slug() {
        let aliases = workspace_label_aliases("flywheel-omp-repro", None);
        assert!(
            aliases.iter().any(|a| a == "flywheel-omp-repro"),
            "raw token must resolve: {aliases:?}"
        );
        assert!(
            aliases
                .iter()
                .any(|a| a == "flywheel-flywheel-omp-repro" || a.starts_with("flywheel-")),
            "default swarm label present: {aliases:?}"
        );
    }

    #[test]
    fn workspace_label_aliases_include_spawn_meta() {
        let aliases = workspace_label_aliases("flywheel", Some("captain-audit"));
        assert!(
            aliases.iter().any(|a| a == "captain-audit"),
            "custom --label must resolve: {aliases:?}"
        );
    }

    #[test]
    fn working_agent_names_only_working() {
        use crate::herdr_shim::{AgentInfo, AgentStatus};
        fn agent(name: &str, status: AgentStatus) -> AgentInfo {
            AgentInfo {
                agent: None,
                agent_status: status,
                cwd: None,
                display_agent: None,
                focused: false,
                foreground_cwd: None,
                interactive_ready: true,
                launch_pending: false,
                name: Some(name.into()),
                pane_id: format!("p-{name}"),
                revision: 0,
                state_change_seq: 0,
                state_labels: serde_json::Map::new(),
                tab_id: "t".into(),
                terminal_id: "term".into(),
                terminal_title: None,
                workspace_id: "w".into(),
            }
        }
        let names = working_agent_names(&[
            agent("oc", AgentStatus::Working),
            agent("oc2", AgentStatus::Idle),
            agent("oc3", AgentStatus::Done),
        ]);
        assert_eq!(names, vec!["oc".to_string()]);
    }
}

#[cfg(test)]
mod workspace_origin_tests {
    use super::*;
    use crate::herdr_shim::{AgentInfo, WorkspaceInfo};

    fn ws(id: &str, label: &str, panes: u64) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.to_string(),
            number: 0,
            label: label.to_string(),
            focused: false,
            pane_count: panes,
            tab_count: 0,
            active_tab_id: String::new(),
            agent_status: AgentStatus::Idle,
            cwd: None,
        }
    }

    fn agent(name: &str, workspace_id: &str) -> AgentInfo {
        AgentInfo {
            agent: Some("opencode".into()),
            agent_status: AgentStatus::Idle,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: format!("{workspace_id}:p1"),
            revision: 1,
            state_change_seq: 0,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term_1".into(),
            terminal_title: None,
            workspace_id: workspace_id.to_string(),
        }
    }

    // bd-627z.3 gate: a foreign workspace (captain/omp session labeled like
    // the project) is NOT flywheel-owned — spawn must warn + proceed.
    #[test]
    fn base_label_alone_is_foreign() {
        let w = ws("w17", "toron", 2);
        assert_eq!(
            classify_workspace_origin("toron", &w, &[], None),
            Some(WorkspaceOrigin::Foreign)
        );
    }

    #[test]
    fn swarm_label_is_owned() {
        let w = ws("w2A", "flywheel-toron", 3);
        assert_eq!(
            classify_workspace_origin("toron", &w, &[], None),
            Some(WorkspaceOrigin::Owned)
        );
    }

    #[test]
    fn flywheel_prefixed_agents_mark_owned() {
        let w = ws("w9", "custom-swarm-label", 1);
        let agents = vec![agent("flywheel-toron-oc", "w9")];
        assert_eq!(
            classify_workspace_origin("toron", &w, &agents, None),
            Some(WorkspaceOrigin::Owned)
        );
    }

    #[test]
    fn recorded_spawn_meta_label_marks_owned() {
        let w = ws("w9", "my-special-label", 1);
        assert_eq!(
            classify_workspace_origin("toron", &w, &[], Some("my-special-label")),
            Some(WorkspaceOrigin::Owned)
        );
    }

    #[test]
    fn unrelated_workspace_is_skipped() {
        let w = ws("w1", "scratchpad", 1);
        assert_eq!(classify_workspace_origin("toron", &w, &[], None), None);
    }

    #[test]
    fn base_label_with_plain_agent_stays_foreign() {
        let w = ws("w17", "toron", 1);
        let agents = vec![agent("oc", "w17")];
        assert_eq!(
            classify_workspace_origin("toron", &w, &agents, None),
            Some(WorkspaceOrigin::Foreign)
        );
    }

    #[test]
    fn spawn_meta_beats_base_label() {
        let w = ws("w2B", "toron", 1);
        assert_eq!(
            classify_workspace_origin("toron", &w, &[], Some("toron")),
            Some(WorkspaceOrigin::Owned)
        );
    }

    #[test]
    fn flywheel_agents_beat_base_label() {
        let w = ws("w17", "toron", 1);
        let agents = vec![agent("flywheel-toron-oc", "w17")];
        assert_eq!(
            classify_workspace_origin("toron", &w, &agents, None),
            Some(WorkspaceOrigin::Owned)
        );
    }

    #[test]
    fn partition_splits_owned_foreign_and_drops_unrelated() {
        let ws_list = vec![
            ws("wA", "flywheel-toron", 2),
            ws("wB", "toron", 1),
            ws("wC", "scratch", 1),
        ];
        let split = partition_workspaces("toron", ws_list, &[], None);
        assert_eq!(split.owned.len(), 1);
        assert_eq!(split.owned[0].workspace_id, "wA");
        assert_eq!(split.foreign.len(), 1);
        assert_eq!(split.foreign[0].workspace_id, "wB");
    }

    // bd-bug-add-foreign-workspace-m7dg gate: `add` targets the flywheel-OWNED
    // swarm workspace only — a foreign captain workspace that merely shares the
    // project base label (w37, label toron) is never selected. This is the
    // regression fixture: the old find_workspace lookup matched the base label
    // and spawned added panes inside the captain's own workspace.
    #[test]
    fn add_targets_owned_workspace() {
        let ws_list = vec![
            ws("w37", "toron", 6),
            ws("w83", "flywheel-toron", 3),
            ws("wX", "scratchpad", 1),
        ];
        let split = partition_workspaces("toron", ws_list, &[], None);
        assert_eq!(split.foreign.len(), 1);
        assert_eq!(split.foreign[0].workspace_id, "w37");
        assert_eq!(split.owned.len(), 1);
        assert_eq!(split.owned[0].workspace_id, "w83");
        let target = add_target_from_split(&split, &[], "toron").unwrap();
        assert_eq!(target, "w83", "add must target the owned swarm workspace");
    }

    // bd-bug-add-foreign-workspace-m7dg: no owned workspace -> add REFUSES
    // (spawn first); a foreign base-labeled workspace alone never suffices.
    #[test]
    fn add_refuses_when_only_foreign_workspace_exists() {
        let ws_list = vec![ws("w37", "toron", 6)];
        let split = partition_workspaces("toron", ws_list, &[], None);
        let err = add_target_from_split(&split, &[], "toron").unwrap_err();
        assert!(
            err.to_string().contains("no flywheel-owned swarm"),
            "expected refuse, got: {err}"
        );
    }
}

#[allow(clippy::too_many_arguments)] // clap command handler: one param per CLI flag.
/// bd-bug-omp-herdr-wedge: harnesses whose herdr integration holds FULL
/// lifecycle-hook authority (herdr skips screen/process detection for them)
/// The neutral boot prompt that opens the agent.prompt gate. H2
/// (bd-harness-pane-send-text-7fe8): delivered via `pane.send_text` after the
/// TUI owns the foreground (works for any kind, non-agent panes too), not as
/// a positional MESSAGES launch arg.
///
/// Lifecycle-authority kinds — pi/omp/kimi/opencode/kilo/mastracode (+grok) —
/// gate agent.prompt on a processed prompt when their herdr integration is
/// reporting; the prime is harmless for all kinds and removes the
/// detection-class race entirely (bd-bug-omp-herdr-wedge, H5
/// bd-harness-prime-kind-list-eivx).
fn boot_prime_line() -> &'static str {
    "Boot ready check: reply exactly READY and await captain instructions."
}

/// Kinds that require the prime as a positional launch arg (secondary path —
/// kept for kinds that cannot take typed TUI input). Primary is
/// [`prime_via_send_text`]; currently no kind needs the in-arg form.
fn needs_in_arg_prime(_kind: &str) -> bool {
    false
}

/// Prime the agent.prompt gate via `pane.send_text` + Enter (H2): replaces
/// the char-by-char send-keys hack (send-keys rejects multi-char text:
/// `invalid_key`) and the MESSAGES-arg boot prime. Best-effort — a failed
/// prime warns but never fails the spawn.
fn prime_via_send_text(c: &mut Client, pane_id: &str) {
    let res = c
        .pane_send_text(pane_id, boot_prime_line())
        .and_then(|_| c.pane_send_input(pane_id, &["Enter"]));
    if let Err(e) = res {
        eprintln!("flywheel: warn: boot-prime via pane.send_text failed for {pane_id}: {e:#}");
    }
}

/// H3 (bd-harness-integration-preflight): lifecycle-hook-authority harness
/// kinds — the same list the boot-prime machinery documents (see
/// [`boot_prime_line`]): their herdr integration authors idle/working/blocked
/// from hook events, so a missing integration degrades lifecycle-state
/// reporting. Warn-only by H3 decision; the env + prime machinery works
/// regardless.
const LIFECYCLE_AUTHORITY_KINDS: &[&str] = &[
    "pi",
    "omp",
    "kimi",
    "opencode",
    "kilo",
    "mastracode",
    "grok",
];

/// H3 (bd-harness-integration-preflight): before a kind's first agent
/// launches, make sure its herdr integration exists and is current.
/// Best-effort by contract — a missing/broken integration NEVER fails the
/// spawn: install failures get the bd-627z.2 loud-warning treatment, and
/// lifecycle-hook-authority kinds additionally warn that lifecycle-state
/// reporting is degraded (the env + boot-prime machinery works regardless —
/// H3 decided warn-only, do NOT block).
fn preflight_kind_integration(
    c: &mut Client,
    kind: &str,
    statuses: &HashMap<String, herdr_shim::IntegrationStatus>,
) {
    if herdr_shim::integration_target(kind).is_none() {
        return; // custom kind with no herdr integration — nothing to preflight
    }
    let st = statuses
        .get(kind)
        .cloned()
        .unwrap_or_else(herdr_shim::IntegrationStatus::not_installed);
    if st.installed && st.current {
        return; // already good
    }
    if st.installed {
        // outdated (vN < vM) — refresh to current
        let detail = match (&st.version, &st.expected_version) {
            (Some(v), Some(e)) => format!("outdated ({v} < {e})"),
            _ => "outdated".to_string(),
        };
        eprintln!("flywheel: warn: {kind} herdr integration {detail} — refreshing");
    }
    match herdr_shim::integration_install_socket(c, kind) {
        Ok(messages) => {
            eprintln!(
                "flywheel: herdr integration {kind} installed via socket{}",
                if messages.is_empty() {
                    String::new()
                } else {
                    format!(": {}", messages.join("; "))
                }
            );
        }
        Err(e) => {
            warn_integration_install_failed(kind, LIFECYCLE_AUTHORITY_KINDS.contains(&kind), &e)
        }
    }
}

/// H3: the bd-627z.2 loud-warning pattern for a missing/outdated integration
/// whose auto-install failed. Hook-authority kinds additionally warn that
/// lifecycle-state reporting is degraded — warn-only, never block.
fn warn_integration_install_failed(kind: &str, hook_authority: bool, e: &anyhow::Error) {
    eprintln!(
        "\n!!! WARNING: the herdr integration for {kind} is missing or outdated and the \
         auto-install failed.\n\
         Fix: run `herdr integration install {kind}` manually \
         (herdr.dev/docs/preview/integrations/).\n\
         {kind} detection falls back to process-based heuristics until the integration reports.\n\
         (integration.install via socket failed: {e:#})\n"
    );
    if hook_authority {
        eprintln!(
            "    {kind} is a lifecycle-hook-authority harness: without a reporting integration,\n\
             herdr cannot author idle/working/blocked from hook events, so lifecycle-state\n\
             reporting is DEGRADED. The env + boot-prime machinery works regardless — the\n\
             spawn proceeds (H3 warn-only, do not block).\n"
        );
    }
}

/// One `herdr integration status` read per spawn/add — the preflight map all
/// kinds share. A failed read degrades to an empty map (every kind looks
/// not-installed → best-effort install attempt, the self-healing path).
fn read_integration_statuses() -> HashMap<String, herdr_shim::IntegrationStatus> {
    match herdr_shim::integration_status_all() {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            eprintln!(
                "flywheel: WARNING cannot read `herdr integration status` — spawn integration \
                 preflight degraded: {e:#}"
            );
            HashMap::new()
        }
    }
}

/// H3: one `kind: <state>` line for the status/deps surfaces
/// (kind -> installed/current/version), painted by state.
fn integration_status_line(kind: &str, st: &herdr_shim::IntegrationStatus) -> String {
    let label = if st.installed && st.current {
        format!("current ({})", st.version.as_deref().unwrap_or("?"))
    } else if st.installed {
        format!(
            "outdated ({} < {})",
            st.version.as_deref().unwrap_or("?"),
            st.expected_version.as_deref().unwrap_or("?")
        )
    } else {
        "not installed".to_string()
    };
    let painted = if st.installed && st.current {
        paint_ok(&label)
    } else if st.installed {
        crate::output::warn(&label)
    } else {
        error(&label)
    };
    format!("{} integration: {}", kind_paint(kind), painted)
}

fn cmd_spawn(
    cfg: &Config,
    project: &str,
    counts: &AgentCounts,
    recipe: Option<&str>,
    kinds: &[(String, u32)],
    label_override: Option<&str>,
    no_focus: bool,
    no_verify: bool,
    retire_orphans: bool,
    peer_aware: bool,
    no_peer_aware: bool,
    peer_poll_interval_minutes: u32,
    peer_poll_budget_minutes: u32,
    peer_max_prompts: u32,
    settle_secs: u64,
    profile_name: Option<&str>,
    profile_count: u32,
) -> Result<()> {
    let dir = project_dir(cfg, project);
    if !dir.exists() {
        // bd-vik B: fail BEFORE any herdr side effect, and name the valid
        // projects so an invented project name is a fast, clear mistake.
        let mut msg = format!(
            "project dir missing: {} (run: flywheel init {project})",
            dir.display()
        );
        if let Some(parent) = dir.parent().filter(|p| p.is_dir())
            && let Ok(rd) = std::fs::read_dir(parent)
        {
            let mut names: Vec<String> = rd
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .filter(|n| !n.starts_with('.'))
                .collect();
            names.sort();
            if !names.is_empty() {
                msg.push_str(&format!(
                    "
projects available: {}",
                    names
                        .iter()
                        .take(10)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        bail!("{msg}");
    }
    // bd-eign.13 fix: a PROFILE-ONLY spawn must not die in recipe_counts'
    // "nothing to spawn" guard — the profile plan is the spawn plan.
    let plan = match recipe_counts(recipe, counts, kinds) {
        Ok(p) => p,
        Err(_e) if profile_name.is_some() => Vec::new(),
        Err(e) => return Err(e),
    };
    // bd-eign.13: merge an optional profile-driven plan (harness kind +
    // count) with the recipe/count/kind plan for mixed crews.
    let catalog = profile::ProfileCatalog::load()?;
    let profile_plan = match profile_name {
        Some(p) => {
            let profile = catalog
                .get(p)
                .ok_or_else(|| anyhow!("unknown profile '@{p}' (flywheel profile list)"))?;
            Some((p, profile, profile_count))
        }
        None => None,
    };
    let items = spawn_items_from_plan(plan, profile_plan);
    if items.is_empty() {
        bail!("nothing to spawn: pass --oc/--pi/--gk counts, --recipe NAME, or --profile NAME");
    }
    let label = label_override
        .map(String::from)
        .unwrap_or_else(|| swarm_label(project));
    // bd-vik C: labels are workspace metadata — normalize to a slug
    // (lowercase alphanumerics + hyphens) and warn when changed.
    let (label, changed) = slugify_label(&label);
    if changed && label_override.is_some() {
        eprintln!(
            "flywheel: label {label_override:?} normalized to {label:?} (slugs: lowercase + hyphens)"
        );
    }
    if label.is_empty() {
        bail!("workspace label must contain at least one alphanumeric character");
    }
    // bd-vik: record the custom label so project-based discovery still works
    // after a custom-label spawn (find_workspace falls back to this).
    if label_override.is_some()
        && let Err(e) = state::save_spawn_meta(project, &label)
    {
        eprintln!("flywheel: cannot save spawn label metadata: {e:#}");
    }

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    // bd-fw-stale-workspace-panes-hgp + bd-627z.3: refuse to spawn over
    // pre-existing FLYWHEEL-OWNED workspaces of this project (stale panes from
    // a killed swarm keep executing the same bead backlog and race the new
    // swarm). --retire-orphans closes them first — the explicit escape hatch.
    // A foreign workspace (captain/omp session labeled like the project) is
    // NOT the stale-pane race — warn and proceed, never close it.
    let split = project_workspaces(&mut c, project)?;
    if !split.foreign.is_empty() {
        let detail: Vec<String> = split
            .foreign
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: warn: {} foreign workspace(s) share the {project} label but are not flywheel-owned — leaving them alone: {}",
            split.foreign.len(),
            detail.join(", ")
        );
    }
    // bd-eign.3 B: probe the pre-existing flywheel-owned workspaces — a
    // stale swarm's panes (processes dead, shell back at the prompt) are
    // AUTO-CLOSED; only workspaces with a live or unprobeable pane take the
    // refuse/--retire-orphans path (never close a workspace that might be
    // working — probe failures never prove death, bd-1np principle).
    let all_agents = c.agent_list()?;
    let mut probe = Client::connect(Some(&cfg.socket_path())).ok();
    let mut dead_owned: Vec<herdr_shim::WorkspaceInfo> = Vec::new();
    let mut live_owned: Vec<herdr_shim::WorkspaceInfo> = Vec::new();
    for ws in &split.owned {
        let ws_agents: Vec<_> = all_agents
            .iter()
            .filter(|a| a.workspace_id == ws.workspace_id)
            .collect();
        if ws_agents.is_empty() {
            live_owned.push(ws.clone());
            continue;
        }
        // bd-bug-liveness-false-death: a workspace whose agents report
        // working/blocked is LIVE, period — the foreground-process liveness
        // probe is known to false-negative on harnesses that resolve through
        // wrappers (omp via mise) or whose integration processes own the
        // foreground. Probe failures never prove death (bd-1np); a
        // confident wrong probe is worse — never auto-close a working swarm.
        if ws_agents
            .iter()
            .any(|a| matches!(a.agent_status.as_str(), "working" | "blocked"))
        {
            live_owned.push(ws.clone());
            continue;
        }
        let mut all_dead = true;
        for a in &ws_agents {
            let kind = a.agent.as_deref().unwrap_or("opencode");
            let name = a.name.as_deref().unwrap_or("<null>");
            let alive = probe_agent_liveness(&mut probe, &a.pane_id, kind, name);
            if alive != Some(false) {
                all_dead = false;
                break;
            }
        }
        if all_dead {
            dead_owned.push(ws.clone());
        } else {
            live_owned.push(ws.clone());
        }
    }
    if !dead_owned.is_empty() {
        let detail: Vec<String> = dead_owned
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: closing {} stale workspace(s) for {project} — all panes process-dead (bd-eign.3 B): {}",
            dead_owned.len(),
            detail.join(", ")
        );
        for ws in &dead_owned {
            if let Err(e) = c.workspace_close(&ws.workspace_id) {
                eprintln!("flywheel: close {} failed: {e:#}", ws.workspace_id);
            }
        }
    }
    let orphans = live_owned;
    if !orphans.is_empty() {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        if retire_orphans {
            eprintln!(
                "flywheel: closing {} pre-existing workspace(s) for {project}: {}",
                orphans.len(),
                detail.join(", ")
            );
            for ws in &orphans {
                if let Err(e) = c.workspace_close(&ws.workspace_id) {
                    eprintln!("flywheel: close {} failed: {e:#}", ws.workspace_id);
                }
            }
        } else {
            bail!(
                "found {} pre-existing workspace(s) for {project} — stale panes from a killed swarm would race the new one: {}. Close them first (flywheel kill {project} or herdr workspace close <id>), or pass --retire-orphans to close them automatically.",
                orphans.len(),
                detail.join(", ")
            );
        }
    }
    // bd-bug-omp-herdr-wedge: herdr's agent.start launches the harness
    // WITHOUT the HERDR_* env its own omp integration contract requires
    // (agent.start has no env param). omp's herdr extension stays disabled,
    // herdr skips screen detection for omp (full lifecycle hook authority),
    // and every agent.prompt gates on agent_not_ready. Inject the env at
    // workspace creation (the one surface herdr's API accepts it); the
    // extension then activates ("Herdr active") and reports on the first
    // processed prompt (see BOOT_PRIME in the spawn loop below).
    let herdr_env = herdr_shim::herdr_integration_env(cfg);
    let (ws, root) = c.workspace_create(
        Some(&utf8_path(&dir, "workspace dir")?),
        Some(&label),
        !no_focus,
        Some(&herdr_env),
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
    // bd-fw-name-binding-loss-7rc: pane→name pairs for the spawn registry.
    let mut registry: Vec<state::SpawnEntry> = Vec::new();
    let mut prev = root.pane_id.clone();
    let mut first = true;
    // ordinals per kind across the whole plan (recipe + flags may repeat a kind)
    let mut ordinals: BTreeMap<String, u32> = BTreeMap::new();
    // peer-aware bootstrap prompt (bd-pattern-peer-triggered, ADR-0001 §9.3).
    // When --peer-aware, each spawned agent receives the self-poll template as
    // its initial instruction. The template is read once; per-agent
    // placeholders are substituted at the call site below.
    // 2026-08-15 (captain): collab is the default — crews self-poll the
    // mailbox unless explicitly opted out. Manual steering is not a swarm.
    let peer_aware = peer_aware && !no_peer_aware;
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
    // H3 (bd-harness-integration-preflight): one status read per spawn;
    // preflight each kind before its first agent launches (deduped — a
    // recipe + explicit flags may repeat a kind).
    let integration_statuses = read_integration_statuses();
    let mut preflighted_kinds: HashSet<String> = HashSet::new();
    for item in &items {
        let kind = item
            .kind
            .clone()
            .unwrap_or_else(|| cfg.resolve_kind(&item.short));
        if preflighted_kinds.insert(kind.clone()) {
            preflight_kind_integration(&mut c, &kind, &integration_statuses);
        }
        for _ in 0..item.count {
            let ordinal = ordinals.entry(item.short.clone()).or_insert(0);
            *ordinal += 1;
            let name = agent_name_n(project, &item.short, *ordinal);
            let pane_id = if first {
                first = false;
                prev.clone()
            } else {
                let p = herdr_shim::pane_split(
                    &mut c,
                    &prev,
                    "down",
                    Some(&utf8_path(&dir, "workspace dir")?),
                    Some(&herdr_env),
                )?;
                prev = p.pane_id.clone();
                prev.clone()
            };
            // S9 (ADR-0018 D1a): register the adj+noun PIN mail identity
            // BEFORE the launch prompt is rendered — the template injects
            // "your mail identity is X" so the agent boots knowing the name
            // it is addressed by. Best-effort (register failures never fail
            // the spawn).
            let mail_identity = register_spawned_agent(cfg, project, &name, &kind, &pane_id);
            let identity = mail_identity.as_deref().unwrap_or(&name);
            let peer_args: Vec<String> = peer_template
                .as_ref()
                .map(|t| {
                    vec![peer_prompt_template(
                        t,
                        &name,
                        identity,
                        peer_poll_interval_minutes,
                        peer_poll_budget_minutes,
                        peer_max_prompts,
                    )]
                })
                .unwrap_or_default();
            // bd-eign.9: a profiled spawn launches with the profile's model —
            // per-harness model args (headless::model_args) + omp role flags,
            // then any launch text (peer template) LAST so a one-shot-capable
            // harness does not swallow the flags as a prompt.
            let mut agent_args: Vec<String> = match &item.profile {
                Some(pname) => {
                    let p = catalog
                        .get(pname)
                        .expect("profile resolved at plan build (bd-eign.13)");
                    profile_launch_args(&kind, p)
                }
                None => vec![],
            };

            let has_peer_prime = !peer_args.is_empty();
            agent_args.extend(peer_args);
            // H2 (bd-harness-pane-send-text-7fe8): the boot prime is typed
            // into the pane via pane.send_text AFTER the TUI owns the
            // foreground + settle (see below) — works for any kind and for
            // non-agent panes, replacing the old positional MESSAGES-arg
            // prime (bd-bug-omp-herdr-wedge). Secondary in-arg path kept for
            // kinds that need it (needs_in_arg_prime; currently none). A
            // supplied peer template IS the prime — never stack.
            if needs_in_arg_prime(&kind) && !has_peer_prime {
                agent_args.push(boot_prime_line().to_string());
            }
            let agent = start_agent_with_retry(&mut c, &name, &kind, &pane_id, &agent_args)?;
            // bd-eign.6 L1: readiness gate — agent.start returns when the
            // launch command is dispatched, not when the TUI is input-ready;
            // a prompt in the boot window is lost to the shell. Poll until
            // the agent binary owns the foreground (max 15s).
            let ready = herdr_shim::wait_agent_foreground(
                &mut c,
                &pane_id,
                &kind,
                &name,
                Duration::from_secs(15),
            )?;
            if !ready {
                eprintln!(
                    "flywheel: warn: {name} not foreground within 15s of spawn — the boot window race (bd-eign.6) still applies; prompts dispatched before the TUI is up may be lost"
                );
            }
            // bd-eign.6 L2: settle — a TUI can own the foreground while MCP
            // servers still initialize; hold dispatch-side sleeps before the
            // spawn command returns (default 30s, observed boot 20-30s).
            if settle_secs > 0 {
                std::thread::sleep(Duration::from_secs(settle_secs));
            }
            // H2 (bd-harness-pane-send-text-7fe8): prime the agent.prompt
            // gate via pane.send_text once the TUI is input-ready (a prompt
            // in the boot window is lost to the shell, bd-eign.6). Universal
            // — any kind, non-agent panes too. Peer templates are their own
            // prime; never stack.
            if !has_peer_prime {
                prime_via_send_text(&mut c, &pane_id);
            }
            // governed-by: ADR-0001 (mail-register-on-spawn) — the spawned
            // agent was already registered (S8.3, above, before the launch
            // prompt); pin the returned identity in the spawn registry
            // (pane → identity) so assign resolves deterministically.
            // bd-627z.4: the pinned identity never shifts across calls,
            // restarts, and daemon restarts.
            let entry = match (&item.profile, &mail_identity) {
                (Some(p), mid) => state::SpawnEntry::profiled(
                    agent.pane_id.clone(),
                    name.clone(),
                    p.clone(),
                    kind.clone(),
                    mid.clone(),
                ),
                (None, Some(mid)) => state::SpawnEntry::with_mail_identity(
                    agent.pane_id.clone(),
                    name.clone(),
                    mid.clone(),
                ),
                (None, None) => state::SpawnEntry::legacy(agent.pane_id.clone(), name.clone()),
            };
            registry.push(entry);
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

    // bd-fw-name-binding-loss-7rc: persist pane→name bindings so a lost name
    // (agent restarted in its pane) can be restored by `flywheel rebind` or
    // the __send auto-heal. Best-effort — a registry failure is not a spawn
    // failure.
    if let Err(e) = state::save_spawn_registry(project, &registry) {
        eprintln!("flywheel: cannot save spawn registry: {e:#}");
    }
    // bd-eign.3 A: project-local mirror of the bindings — survives a state
    // dir wipe; `cmd_recover` falls back to it when the registry is empty.
    if let Err(e) = state::save_agent_names(&dir, &registry) {
        eprintln!("flywheel: cannot save .flywheel/agent-names.json: {e:#}");
    }

    // bd-vik A: post-spawn visibility check — herdr's session registry
    // (session.json) can silently fail to persist, leaving the panes LIVE but
    // invisible to list/send/status. Verify the exact workspace_id we just
    // created is visible from the same view list/send use; fail loudly with
    // recovery steps instead of silent success.
    if !no_verify {
        let listed = c.workspace_list()?;
        let created_id = ws.workspace_id.clone();
        if !listed.iter().any(|lws| lws.workspace_id == created_id) {
            bail!(
                "spawn created workspace {created_id} but `flywheel list` cannot see it — herdr's session registry (session.json) failed to persist the workspace.
recovery: restart herdr (launchctl kickstart -k gui/$(id -u)/com.herdr... or `herdr restart`), then re-run: flywheel spawn {project}
the panes may still be live in herdr; `--no-verify` skips this check in scripts"
            );
        }
    }

    // bd-4pxy: spawn readiness — "swarm ready" must never print while an
    // agent is still launch_pending (herdr: the launcher command has not
    // finished; a harness that cannot start stays pending). Wait briefly,
    // then FAIL naming the field + the pending agents (a silent ready is
    // the bug that produced dead sends).
    if !no_verify {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let agents = c.agent_list()?;
            let pending: Vec<AgentInfo> = agents
                .iter()
                .filter(|a| a.workspace_id == ws.workspace_id && is_still_booting(a))
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "{}",
                    launch_pending_error(&pending).expect("pending non-empty here")
                );
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

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

/// H1 (bd-harness-prime-agent-818f): herdr has NO kind for some harnesses —
/// its `Agent` enum is a closed 22-kind set (`agent.start --kind pa` →
/// `unsupported_agent_kind`), and `server.agent_manifests` registers
/// screen-detection rules only, NOT kinds. A harness WITH a spawn recipe
/// (`[harness.<name>]`, compiled-in or profiles.toml) is launched via the
/// recipe instead: `pane.run` the command. herdr's standard pane launch env
/// already carries HERDR_ENV / HERDR_SOCKET_PATH / HERDR_PANE_ID, so
/// prime-agent's built-in herdr extension activates and self-reports
/// lifecycle (working/blocked/idle) through the socket — which herdr accepts
/// as hook authority even for unknown kinds (empirically verified: the pane
/// shows up in `agent list` as `agent=prime-agent`). The normal wait/rename
/// loop then binds the name. Returns `Ok(true)` when the recipe path was
/// taken, `Ok(false)` when this kind has no recipe, `Err` on launch failure.
fn try_recipe_launch(c: &mut Client, kind: &str, pane_id: &str, args: &[String]) -> Result<bool> {
    // Best-effort catalog: a malformed user profiles.toml must not convert a
    // spawn into a hard error — the recipe is a fallback, not a gate.
    let Some(recipe) = ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.harness(kind).and_then(|h| h.spawn_cmd.clone()))
    else {
        return Ok(false);
    };
    let cmd = recipe_launch_command(&recipe, args);
    eprintln!("agent.start: herdr has no kind '{kind}'; launching via harness recipe: {cmd}");
    c.pane_run(pane_id, &cmd)?;
    Ok(true)
}

/// Trait-object variant of [`try_recipe_launch`] (same contract as
/// [`start_agent_with_retry_dyn`]): routes the recipe launch through
/// `MultiplexerBackend::pane_run`.
fn try_recipe_launch_dyn(
    backend: &dyn backend::MultiplexerBackend,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<bool> {
    let Some(recipe) = ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.harness(kind).and_then(|h| h.spawn_cmd.clone()))
    else {
        return Ok(false);
    };
    let cmd = recipe_launch_command(&recipe, args);
    eprintln!("agent.start: herdr has no kind '{kind}'; launching via harness recipe: {cmd}");
    backend.pane_run(pane_id, &cmd)?;
    Ok(true)
}

/// Shell-quote one arg for the `pane.run` command line (single quotes,
/// escaping embedded quotes the POSIX way).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Render a `[harness.<name>] spawn_cmd` recipe + launch args into a single
/// shell command. Args are shell-quoted so model ids / boot primes with
/// spaces survive the pane's shell.
fn recipe_launch_command(recipe: &str, args: &[String]) -> String {
    let mut cmd = recipe.to_string();
    for a in args {
        cmd.push(' ');
        cmd.push_str(&shell_quote(a));
    }
    cmd
}

#[cfg(test)]
mod recipe_launch_tests {
    use super::{recipe_launch_command, shell_quote};

    #[test]
    fn shell_quote_wraps_plain_and_escapes_embedded_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b c"), "'a b c'");
        // POSIX single-quote escape: '\'' ends the quote, emits a literal
        // quote, reopens.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn recipe_command_appends_quoted_args() {
        let args: Vec<String> = vec![
            "--model".into(),
            "opencode-go/deepseek-v4-flash".into(),
            "Boot ready check: reply exactly READY.".into(),
        ];
        assert_eq!(
            recipe_launch_command("prime-agent --offline", &args),
            "prime-agent --offline '--model' 'opencode-go/deepseek-v4-flash' 'Boot ready check: reply exactly READY.'"
        );
    }

    #[test]
    fn recipe_command_with_no_args_is_the_recipe() {
        assert_eq!(
            recipe_launch_command("prime-agent --offline", &[]),
            "prime-agent --offline"
        );
    }
}

pub(crate) fn start_agent_with_retry(
    c: &mut Client,
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
) -> Result<AgentInfo> {
    let mut recipe_launched = false;
    for attempt in 0..30 {
        if !recipe_launched {
            match c.agent_start(name, kind, pane_id, 45_000, args) {
                Ok(_) => {}
                Err(e) => {
                    if attempt == 0 {
                        eprintln!("agent.start error: {e:#}");
                        // Deterministic herdr rejection for a kind it does not
                        // know: fall back to the harness spawn recipe once.
                        // No recipe -> fail fast instead of burning 30 retries.
                        if matches!(&e.code, BackendErrorCode::Other(c) if c == "unsupported_agent_kind")
                        {
                            if try_recipe_launch(c, kind, pane_id, args)? {
                                recipe_launched = true;
                            } else {
                                return Err(e.into());
                            }
                        }
                    }
                }
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
                if a.name.as_deref() != Some(name)
                    && let Err(e) = c.agent_rename(&a.pane_id, name)
                {
                    eprintln!("agent.rename error: {e:#}");
                }
                if let Ok(agents) = c.agent_list()
                    && let Some(a) = agents.iter().find(|a| a.pane_id == pane_id)
                {
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
    let mut recipe_launched = false;
    for attempt in 0..30 {
        if !recipe_launched {
            match backend.agent_start(name, kind, pane_id, args) {
                Ok(_) => {}
                Err(e) => {
                    if attempt == 0 {
                        eprintln!("agent.start error: {e:#}");
                        // H1 (bd-harness-prime-agent-818f): same recipe
                        // fallback as the concrete-client variant — herdr
                        // rejects unknown kinds deterministically.
                        if matches!(&e.code, BackendErrorCode::Other(c) if c == "unsupported_agent_kind")
                        {
                            if try_recipe_launch_dyn(backend, kind, pane_id, args)? {
                                recipe_launched = true;
                            } else {
                                return Err(e.into());
                            }
                        }
                    }
                }
            }
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
                if let Ok(agents) = backend.agent_list()
                    && let Some(a) = agents.iter().find(|a| a.pane_id == pane_id)
                {
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
/// bd-627z.2: a CONFIGURED-but-unreachable daemon (the [mail.toron] url is
/// set, the daemon is down) is NOT a silent skip — it gets a prominent
/// warning that the agents will have no mail identities until the daemon
/// recovers, with the remediation (start the daemon; the loop/assign
/// auto-registers on first contact once it is back).
///
/// bd-627z.4: returns the DETERMINISTIC mail identity so the spawn caller
/// can pin it in the spawn registry (pane → identity) — assign then
/// resolves through that map and the identity never shifts.
///
/// S9 (ADR-0018 D1a): the identity is a UNIQUE adjective+noun PIN (the
/// legacy whitelist generator, collision-checked against the spawn
/// registry pins AND the registered agents; retry on collision), not the
/// canonical herdr name. The slug name stays the technical pane handle and
/// resolves as an alias through the registry pin (wake + debate read it).
/// A re-spawn of the same herdr name KEEPS its pinned identity — idempotent,
/// never a fresh draw. The random npub stays underneath (registration
/// creates or upserts the key for the pin).
///
/// S8.4: the upsert carries a FRESH task stamp (spawn_agent_profile) — the
/// toron register_agent upsert overwrites stale profile task fields from a
/// prior session (a 2026-08-14 identity still advertised `bd-qk9q`).
///
/// governed-by: ADR-0001 (bd-mail-register-on-spawn, bd-mail-pane-identity-bridge), ADR-0002 (factory), ADR-0018 D1a (bd-spawn-adjnoun-pin-7psn)
pub(crate) fn register_spawned_agent(
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
    pane_id: &str,
) -> Option<String> {
    let key = project_dir(cfg, project).to_str().map(str::to_string)?;
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(_) => return None, // mail not configured — silent skip
    };
    // bd-627z.2: configured-remote (a [mail.toron] url is set) but the
    // daemon is unreachable → loud warning, not the old silent eprintln.
    // The in-process embedded mailbox cannot be down, so this only fires
    // for the remote-daemon mode (where a down daemon is a real ops event).
    let daemon_mode = cfg
        .toron_mail_config()
        .url
        .as_deref()
        .map(|u| !u.is_empty())
        .unwrap_or(false);
    let warn_no_identities = |e: &backend::MailError| {
        if daemon_mode {
            eprintln!(
                "\n!!! WARNING: the toron mail daemon is unreachable — spawned \
                 agents will have NO mail identities until it recovers.\n\
                 Fix: start the daemon (toron serve), then the next assign / \
                 loop dispatch auto-registers each identity on first contact.\n\
                 (register_agent failed for {name}: {e})\n"
            );
        } else {
            eprintln!("flywheel: mail register_agent failed for {name}: {e}");
        }
    };
    if let Err(e) = backend::run_mail(mail.ensure_project(&key)) {
        warn_no_identities(&e);
        return None;
    }
    // S9 (ADR-0018 D1a): the mail identity is a UNIQUE adjective+noun pin.
    // The canonical slug stays the herdr handle and resolves as an alias
    // through the registry pin (wake + debate read it). A re-spawn of the
    // same name KEEPS its pinned identity (idempotent); a fresh name draws
    // a pin collision-checked against the spawn-registry pins AND the
    // registered agents table, retrying on collision.
    let registry = state::load_spawn_registry(project);
    let mail_name = registry
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.mail_identity.clone())
        .filter(|m| !m.is_empty() && m != name) // bd-bug-omp-herdr-wedge: a retained SLUG (pre-ADR-0018 D1a registry entry) is not a pin — redraw
        .unwrap_or_else(|| {
            let mut taken: std::collections::HashSet<String> = registry
                .iter()
                .filter_map(|e| e.mail_identity.clone())
                .collect();
            match backend::run_mail(mail.list_agents(&key, None)) {
                Ok(agents) => {
                    taken.extend(agents.into_iter().map(|a| a.name));
                }
                Err(e) => {
                    eprintln!("flywheel: mail list_agents failed during pin draw for {name}: {e}")
                }
            }
            unique_adjnoun_pin(&taken, state::fnv1a64(name))
        });
    let profile = spawn_agent_profile(&mail_name, kind, project);
    if let Err(e) = backend::run_mail(mail.register_agent(&key, &profile)) {
        // Best-effort: a registration failure must never fail the spawn. The
        // pane-identity bridge below still works (the identity file is the
        // pane→name source the server reads, registration or not).
        // bd-627z.2: a configured-but-down daemon gets the loud identity
        // warning (agents will have no mail identities until it recovers).
        warn_no_identities(&e);
        return None;
    }
    // bridge the identity to the herdr pane (best-effort, like register).
    // The file holds the REGISTERED identity (the pin) so
    // resolve_pane_identity returns a name that exists in the registry.
    if let Err(e) = backend::run_mail(mail.write_pane_identity(&key, pane_id, &mail_name)) {
        eprintln!("flywheel: mail write_pane_identity failed for {name} on {pane_id}: {e}");
    }
    // persist the pin binding so a re-spawn of the same herdr name KEEPS the
    // identity (idempotent — never a fresh draw) and wake/debate resolve
    // the slug through it. The spawn command saves the full registry too,
    // but the add path relies on this write.
    let mut entries = state::load_spawn_registry(project);
    match entries.iter_mut().find(|e| e.name == name) {
        Some(e) => e.mail_identity = Some(mail_name.clone()),
        None => entries.push(state::SpawnEntry::with_mail_identity(
            pane_id.to_string(),
            name.to_string(),
            mail_name.clone(),
        )),
    }
    if let Err(e) = state::save_spawn_registry(project, &entries) {
        eprintln!("flywheel: cannot save spawn registry for {name}: {e:#}");
    }
    Some(mail_name)
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
        if let Some(rest) = lower.strip_prefix(adj)
            && !rest.is_empty()
            && nouns.contains(&rest)
        {
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
    // Module-scope: the S9 pin draw (unique_adjnoun_pin) shares the whitelist.
    let h = state::fnv1a64(herdr_name);
    adjnoun_from_fnv(h)
}

/// The legacy whitelist pair for a hash: ADJECTIVES[h % 75] +
/// NOUNS[(h >> 32) % 132], PascalCase. This is the pair shape agent-mail
/// validates (bd-odc) — the "legacy generator" the captain's adj+noun pins
/// are drawn from (ADR-0018 D1a).
///
/// governed-by: ADR-0018 D1a (bd-spawn-adjnoun-pin-7psn)
pub(crate) fn adjnoun_from_fnv(h: u64) -> String {
    let adj = ADJECTIVES[(h % ADJECTIVES.len() as u64) as usize];
    let noun = NOUNS[((h >> 32) % NOUNS.len() as u64) as usize];
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

/// S9 (ADR-0018 D1a): draw a UNIQUE adjective+noun pin, deterministic under
/// `seed` (the caller seeds per herdr name, so the FIRST candidate matches
/// the legacy hash mapping when free). A candidate already in `taken`
/// (spawn-registry pins + registered agents) redraws to the next candidate
/// — the 9900-pair space makes the loop effectively single-pass and the
/// seed increment guarantees termination.
///
/// governed-by: ADR-0018 D1a (bd-spawn-adjnoun-pin-7psn)
pub(crate) fn unique_adjnoun_pin(taken: &std::collections::HashSet<String>, seed: u64) -> String {
    let mut h = seed;
    loop {
        let pin = adjnoun_from_fnv(h);
        if !taken.contains(&pin) {
            return pin;
        }
        h = h.wrapping_add(1);
    }
}

/// Keep in lockstep with mcp_agent_mail_core::models::VALID_ADJECTIVES.
/// (75 adjectives — the S9 pin-draw candidate space.)
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
/// Keep in lockstep with mcp_agent_mail_core::models::VALID_NOUNS.
/// (132 nouns — the S9 pin-draw candidate space.)
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

/// Resolve a mail-recipient specifier to a registered agent-mail identity
/// name. A herdr agent name (`flywheel-<base>-<kind>[N]`) maps
/// deterministically to its adjective+noun identity — registration uses the
/// same mapping (bd-odc), so the identity exists. Anything else passes
/// through unchanged: it is either an already-valid identity (the mapping is
/// idempotent for those) or a recipient the server will reject honestly.
/// Pure — no registry round trips.
pub(crate) fn resolve_mail_target(target: &str) -> String {
    if target.contains('-') {
        agent_mail_name(target)
    } else {
        target.to_string()
    }
}

/// bd-627z.4: resolve a mail-recipient specifier THROUGH the spawn registry
/// first — the identity pinned at spawn (pane → adjective+noun) wins over
/// the pure hash mapping. Numbered agents (oc2/oc3/...) keep the SAME
/// identity across calls, restarts, and daemon restarts because the binding
/// is recorded at spawn, not re-derived from the (renumberable) herdr name.
/// Falls back to [`resolve_mail_target`] when the registry has no pin.
///
/// Registry-miss reasons (the bead's clear-error contract): the target was
/// never spawned in this project, or it is already an identity name. The
/// caller decides how to surface those.
pub(crate) fn resolve_mail_target_registry(project: &str, target: &str) -> String {
    let entries = state::load_spawn_registry(project);
    for e in &entries {
        if e.name == target {
            if let Some(mid) = &e.mail_identity {
                return mid.clone();
            }
            // Legacy entry (no pin): fall through to the hash mapping.
            break;
        }
    }
    resolve_mail_target(target)
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
        assert!(
            err.to_string().contains("not valid UTF-8"),
            "actionable error: {err:#}"
        );
        // a valid path round-trips exactly
        let good = PathBuf::from("/tmp/flywheel-utf8-ok");
        assert_eq!(
            utf8_path(&good, "test path").unwrap(),
            "/tmp/flywheel-utf8-ok"
        );
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

    /// bd-627z.4 GATE: the spawn-registry pin wins over the pure hash —
    /// two resolves of the SAME agent return the SAME identity, the pin
    /// survives a reload (daemon-restart equivalent), and an unpinned agent
    /// falls back to the deterministic hash mapping (still stable).
    #[test]
    fn registry_pin_is_deterministic_and_survives_reload() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-reg-pin-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        let project = "/tmp/fw-regpin-proj";
        let entries = vec![
            state::SpawnEntry::with_mail_identity(
                "w3:p1",
                "flywheel-fw-regpin-oc2",
                "DustySparrow",
            ),
            state::SpawnEntry::with_mail_identity("w3:p2", "flywheel-fw-regpin-oc3", "RoseDune"),
        ];
        state::save_spawn_registry(project, &entries).expect("save");

        // Same agent → same identity, every call.
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc2"),
            "DustySparrow"
        );
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc2"),
            "DustySparrow"
        );
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc3"),
            "RoseDune"
        );

        // The pin survives a full reload (fresh load from disk = the
        // daemon-restart / re-dispatch equivalent — no shifting).
        let entries2 = state::load_spawn_registry(project);
        assert_eq!(entries2.len(), 2);
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc2"),
            "DustySparrow",
            "binding persists across reloads"
        );

        // Unpinned agent → deterministic hash fallback (stable per name).
        let hashed = resolve_mail_target_registry(project, "flywheel-fw-regpin-gk");
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-gk"),
            hashed,
            "fallback is stable"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
    /// double-wrapped. Bare shorts expand; concrete names stay exact
    /// (WantToken.broaden=false) so a full name never drags in oc2.
    #[test]
    fn to_want_token_passes_through_full_names() {
        // full spawned name passes through, no broadening (the fix for bd-bwf)
        assert_eq!(
            to_want_token("toron", "flywheel-toron-oc"),
            WantToken {
                name: "flywheel-toron-oc".into(),
                broaden: false,
                short: None,
                ordinal: 1,
            }
        );
        assert_eq!(
            to_want_token("toron", "flywheel-toron-oc2"),
            WantToken {
                name: "flywheel-toron-oc2".into(),
                broaden: false,
                short: None,
                ordinal: 1,
            }
        );
        // numbered short: exact, no broadening to oc20; carries kind+ordinal
        // for the bd-4pxy kind fallback
        assert_eq!(
            to_want_token("toron", "oc2"),
            WantToken {
                name: "flywheel-toron-oc2".into(),
                broaden: false,
                short: Some("oc".into()),
                ordinal: 2,
            }
        );
        // bd-eign.14: oc1 (the first oc agent) resolves — the first agent
        // of a kind is NOT digit-suffixed, so the ordinal applies to the kind
        assert_eq!(
            to_want_token("toron", "oc1"),
            WantToken {
                name: "flywheel-toron-oc".into(),
                broaden: false,
                short: Some("oc".into()),
                ordinal: 1,
            }
        );
        assert_eq!(
            to_want_token("toron", "pi2"),
            WantToken {
                name: "flywheel-toron-pi2".into(),
                broaden: false,
                short: Some("pi".into()),
                ordinal: 2,
            }
        );
        assert_eq!(
            to_want_token("toron", "gk1"),
            WantToken {
                name: "flywheel-toron-gk".into(),
                broaden: false,
                short: Some("gk".into()),
                ordinal: 1,
            }
        );
        // a bare digit is not a numbered short (no kind prefix)
        assert_eq!(
            to_want_token("toron", "2"),
            WantToken {
                name: "flywheel-toron-2".into(),
                broaden: false,
                short: Some("2".into()),
                ordinal: 1,
            }
        );
        // bare shorts expand to first ordinal AND broaden to numbered agents
        assert_eq!(
            to_want_token("toron", "oc"),
            WantToken {
                name: "flywheel-toron-oc".into(),
                broaden: true,
                short: Some("oc".into()),
                ordinal: 1,
            }
        );
        assert_eq!(
            to_want_token("toron", "pi"),
            WantToken {
                name: "flywheel-toron-pi".into(),
                broaden: true,
                short: Some("pi".into()),
                ordinal: 1,
            }
        );
        // raw pane id passes through
        assert_eq!(
            to_want_token("toron", "w1:p1"),
            WantToken {
                name: "w1:p1".into(),
                broaden: false,
                short: None,
                ordinal: 1,
            }
        );
        // empty passes through (caller validates distinctness)
        assert_eq!(
            to_want_token("toron", ""),
            WantToken {
                name: "".into(),
                broaden: false,
                short: None,
                ordinal: 1,
            }
        );
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
    // bd-bug-add-foreign-workspace-m7dg: add targets the flywheel-OWNED swarm
    // workspace only (same partition as spawn). find_workspace matched the
    // project base label and spawned panes into a foreign captain workspace
    // (w37, label toron); that path is now refused/never targeted.
    let ws_id = add_target_workspace(&mut c, project)?;
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
    // bd-bug-omp-herdr-wedge: split panes do NOT inherit the workspace env —
    // pass the herdr integration env at split time too (same as the spawn
    // path's workspace.create injection).
    let herdr_env = herdr_shim::herdr_integration_env(cfg);
    // H3 (bd-harness-integration-preflight): same preflight as spawn — add
    // launches kinds into an existing workspace, so their integrations must
    // be present too. One status read per add, deduped per kind.
    let integration_statuses = read_integration_statuses();
    let mut preflighted_kinds: HashSet<String> = HashSet::new();
    for (short, n) in &plan {
        let kind = cfg.resolve_kind(short);
        if preflighted_kinds.insert(kind.clone()) {
            preflight_kind_integration(&mut c, &kind, &integration_statuses);
        }
        for _ in 0..*n {
            let ordinal = ordinals.entry(short.clone()).or_insert(0);
            *ordinal += 1;
            let name = agent_name_n(project, short, *ordinal);
            let p = herdr_shim::pane_split(
                &mut c,
                &prev,
                "down",
                Some(&utf8_path(&dir, "workspace dir")?),
                Some(&herdr_env),
            )?;
            prev = p.pane_id.clone();
            let mut add_args: Vec<String> = Vec::new();
            // H2 (bd-harness-pane-send-text-7fe8): same boot-prime contract
            // as spawn — the prime is typed via pane.send_text after the TUI
            // owns the foreground (below), not pushed as a positional
            // MESSAGES arg. In-arg secondary kept for kinds that need it.
            if needs_in_arg_prime(&kind) {
                add_args.push(boot_prime_line().to_string());
            }
            let agent = start_agent_with_retry(&mut c, &name, &kind, &prev, &add_args)?;
            // H2: the add path has no settle — wait for the TUI to own the
            // foreground (bd-eign.6, same 15s contract as spawn) before the
            // send_text prime, so it is not lost to the shell.
            let _ = herdr_shim::wait_agent_foreground(
                &mut c,
                &prev,
                &kind,
                &name,
                Duration::from_secs(15),
            );
            if !needs_in_arg_prime(&kind) {
                prime_via_send_text(&mut c, &prev);
            }
            // bd-627z.4: register + pin the mail identity (the add path's
            // registry persists on the next spawn/rebind save).
            let _ = register_spawned_agent(cfg, project, &name, &kind, &agent.pane_id);
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
            .map(|a| {
                json!({
                    "name": a.name, "kind": a.agent,
                    // bd-4pxy: a launch_pending agent is NOT idle — it is
                    // still booting (the status must never mask the boot).
                    "status": status_label(a.launch_pending, a.agent_status.as_str()),
                    "launch_pending": a.launch_pending,
                })
            })
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

/// Classify only. Never dispatches.
fn cmd_decide(
    target: &str,
    ready_beads: u32,
    profiled_panes: u32,
    launch_pending: u32,
    contention: bool,
    json: bool,
) -> Result<()> {
    let d = decide::decide(
        target,
        &decide::Snapshot {
            ready_bead_count: ready_beads,
            profiled_panes,
            launch_pending,
            contention,
        },
    );
    if json {
        print_json(&serde_json::json!({
            "primitive": d.primitive,
            "reason": d.reason,
            "argv": d.argv,
        }));
    } else {
        println!("{}\n{}\n{}", d.primitive, d.reason, d.argv.join(" "));
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
                "name": a.name, "kind": a.agent,
                // bd-4pxy: a launch_pending agent is NOT idle — still booting.
                "status": status_label(a.launch_pending, a.agent_status.as_str()),
                "pane_id": a.pane_id, "cwd": a.cwd, "workspace_id": ws_id,
                "launch_pending": a.launch_pending,
            })
        })
        .collect();
    // H3 (bd-harness-integration-preflight): surface each distinct kind's
    // herdr integration state (kind -> installed/current/version).
    let integration_statuses = read_integration_statuses();
    let integrations: BTreeMap<String, herdr_shim::IntegrationStatus> = agents
        .iter()
        .filter_map(|a| a.agent.as_deref())
        .filter(|k| herdr_shim::integration_target(k).is_some())
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|k| {
            let st = integration_statuses
                .get(k)
                .cloned()
                .unwrap_or_else(herdr_shim::IntegrationStatus::not_installed);
            (k.to_string(), st)
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
        for (k, st) in &integrations {
            println!("  {}", integration_status_line(k, st));
        }
    } else {
        print_json(&json!({
            "workspace_id": ws_id, "agents": rows, "integrations": integrations
        }));
    }
    Ok(())
}

/// bd-fw-name-binding-loss-7rc: reconcile the spawn registry (pane_id → name)
/// against live panes. A pane whose agent restarted shows `name:null` (or a
/// changed name) — this renames it back to its registered name so send /
/// status / loop resolution works again.
fn cmd_rebind(cfg: &Config, project: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let entries = state::load_spawn_registry(project);
    if entries.is_empty() {
        eprintln!(
            "flywheel: no spawn registry for {project} — re-run `flywheel spawn {project}` to record pane→name bindings"
        );
        print_json(&json!({ "project": project, "rebound": 0, "registry": "empty" }));
        return Ok(());
    }
    let agents = backend.agent_list()?;
    let mut rebound: Vec<Value> = Vec::new();
    let mut ok_count = 0usize;
    let mut stale = 0usize;
    for entry in &entries {
        let (pane_id, name) = (&entry.pane_id, &entry.name);
        let Some(a) = agents
            .iter()
            .find(|a| a.pane_id == *pane_id && a.workspace_id == ws_id)
        else {
            stale += 1; // pane gone (respawned) — a fresh pane carries its name
            continue;
        };
        if a.name.as_deref() == Some(name) {
            ok_count += 1;
            continue;
        }
        let was = a.name.clone().unwrap_or_else(|| "<null>".to_string());
        if let Err(e) = backend.agent_rename(pane_id, Some(name)) {
            eprintln!("flywheel: rebind {pane_id} → {name} failed: {e:#}");
        } else {
            rebound.push(json!({ "pane_id": pane_id, "was": was, "now": name }));
        }
    }
    print_json(&json!({
        "project": project,
        "workspace_id": ws_id,
        "ok": ok_count,
        "rebound": rebound.len(),
        "rebound_details": rebound,
        "stale_entries": stale,
    }));
    Ok(())
}

/// Agent kinds recorded by the last spawn event in the project's event log
/// (name → kind). Used by `flywheel recover` to respawn dead agents with the
/// right kind. Falls back to `opencode` for names not in the log.
fn spawn_kinds_from_log(project: &str) -> std::collections::HashMap<String, String> {
    let mut kinds = std::collections::HashMap::new();
    let path = crate::config::state_dir().join(format!("{project}.jsonl"));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return kinds;
    };
    for line in raw.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("kind").and_then(serde_json::Value::as_str) != Some("spawn") {
            continue;
        }
        let Some(agents) = v
            .get("data")
            .and_then(|d| d.get("agents"))
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for a in agents {
            let name = a
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let kind = a
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("opencode");
            kinds
                .entry(name.to_string())
                .or_insert_with(|| kind.to_string());
        }
        break; // last spawn event only
    }
    kinds
}

/// bd-a2bw: recover must probe/respawn with the registry harness first.
/// Profile coder panes run `omp`; the event-log fallback is `opencode`,
/// so a live omp looks process-dead and recover tries to start a second
/// harness in the same pane.
fn recover_probe_kind<'a>(
    entry: &'a state::SpawnEntry,
    log_kinds: &'a std::collections::HashMap<String, String>,
) -> &'a str {
    entry
        .harness
        .as_deref()
        .filter(|h| !h.is_empty())
        .or_else(|| log_kinds.get(&entry.name).map(String::as_str))
        .unwrap_or("opencode")
}

/// bd-a8i: window for the responsiveness probe — long enough for a live
/// agent's `state_change_seq` to advance on idle churn, short enough that
/// `cmd_recover` stays snappy across many agents.
const RESPONSIVENESS_WINDOW: Duration = Duration::from_secs(3);

/// bd-1np: probe one registered agent's process liveness via herdr
/// `pane.process_info` — pane existence is NOT liveness (a dead agent's pane
/// still lists in herdr with the binding intact). `Some(true)` = process
/// alive, `Some(false)` = provably dead, `None` = probe unavailable or
/// inconclusive (the caller falls back to legacy pane-existence
/// classification — never a confident "healthy").
fn probe_agent_liveness(
    probe: &mut Option<herdr_shim::Client>,
    pane_id: &str,
    kind: &str,
    name: &str,
) -> Option<bool> {
    let c = probe.as_mut()?;
    match herdr_shim::pane_process_info(c, pane_id) {
        Ok(pi) => herdr_shim::pane_agent_alive(&pi, kind),
        Err(e) => {
            eprintln!("flywheel: recover liveness probe for {name} failed: {e:#}");
            None
        }
    }
}

/// One agent's recovery decision in `cmd_recover` (bd-1np): healthy, rebind
/// (pane survived, binding lost), or respawn (pane/process gone) with the
/// reason recorded in the report.
enum RecoverDecision<'a> {
    Healthy,
    Rebind { was: String },
    Respawn { reason: &'a str },
}

/// bd-fw-interrupt-kills-agent-or3 (c): CLI supervision counterpart to the
/// durable loop's self-heal — for every registered agent, restore the name
/// binding if the pane survived, or respawn the agent (with its spawn-time
/// kind) if it died. Reports what was done per agent.
///
/// bd-1np: an intact pane + binding is healthy ONLY if the process inside is
/// alive. Each seemingly-healthy agent is liveness-probed via herdr
/// `pane.process_info` (agent binary in the foreground process group); a pane
/// whose process died (shell owns the foreground) is respawned in place, not
/// classified healthy.
///
/// bd-a8i: process liveness is not responsiveness — a frozen opencode passes
/// the foreground-group check. Alive agents are additionally probed for
/// responsiveness (state_change_seq advancing across a window); a frozen +
/// idle agent is respawned, not classified healthy.
fn cmd_recover(cfg: &Config, project: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    // bd-eign.3 A: registry first; the project-local mirror is the fallback
    // when the state-dir registry is gone (wipe/restore).
    let mut entries = state::load_spawn_registry(project);
    if entries.is_empty() {
        entries = state::load_agent_names(&project_dir(cfg, project));
    }
    if entries.is_empty() {
        eprintln!(
            "flywheel: no spawn registry for {project} — re-run `flywheel spawn {project}` to record pane→name bindings"
        );
        print_json(&json!({ "project": project, "recovered": 0, "registry": "empty" }));
        return Ok(());
    }
    let kinds = spawn_kinds_from_log(project);
    let agents = backend.agent_list()?;
    // bd-1np: concrete herdr client for the process-liveness probe — the
    // backend trait has no `pane.process_info`. Only the herdr backend can
    // probe; other backends fall back to pane-existence classification.
    let mut probe = (backend.name() == "herdr")
        .then(|| Client::connect(Some(&cfg.socket_path())).ok())
        .flatten();
    let mut recovered: Vec<Value> = Vec::new();
    let mut healthy = 0usize;
    for entry in &entries {
        let (pane_id, name) = (&entry.pane_id, &entry.name);
        let kind = recover_probe_kind(entry, &kinds);
        let live = agents
            .iter()
            .find(|a| a.pane_id == *pane_id && a.workspace_id == ws_id);
        let decision = match live {
            Some(a) if a.name.as_deref() == Some(name) => {
                // pane + binding intact — liveness-probe the process inside.
                match probe_agent_liveness(&mut probe, pane_id, kind, name) {
                    // provably dead: respawn in the same pane (its shell is
                    // back at the prompt). Some(false) is the bd-1np signal.
                    Some(false) => RecoverDecision::Respawn {
                        reason: "process-dead",
                    },
                    // process alive — probe responsiveness (bd-a8i): a
                    // frozen agent passes pane_agent_alive but does not
                    // advance state_change_seq while idle.
                    Some(true) => {
                        let verdict = probe.as_mut().map(|c| {
                            herdr_shim::probe_agent_responsiveness(
                                c,
                                pane_id,
                                RESPONSIVENESS_WINDOW,
                            )
                        });
                        match verdict {
                            // alive as a PID, dead to input → respawn,
                            // not healthy (the bd-a8i shape).
                            Some(herdr_shim::Responsiveness::Unresponsive) => {
                                RecoverDecision::Respawn {
                                    reason: "unresponsive",
                                }
                            }
                            // responsive or inconclusive → healthy (a failed
                            // probe never respawns an agent — bd-1np
                            // principle).
                            _ => RecoverDecision::Healthy,
                        }
                    }
                    // probe unavailable/inconclusive → legacy healthy
                    // classification (probe error ≠ dead — never respawn an
                    // agent on a failed probe).
                    None => RecoverDecision::Healthy,
                }
            }
            Some(a) => {
                // pane survived, binding lost → restore it (7rc path)
                RecoverDecision::Rebind {
                    was: a.name.clone().unwrap_or_else(|| "<null>".to_string()),
                }
            }
            None => RecoverDecision::Respawn {
                reason: "agent-gone",
            },
        };
        match decision {
            RecoverDecision::Healthy => {
                healthy += 1; // pane + binding + live process
            }
            RecoverDecision::Rebind { was } => {
                backend.agent_rename(pane_id, Some(name))?;
                recovered.push(json!({
                    "name": name, "pane_id": pane_id, "action": "rebound", "was": was
                }));
            }
            RecoverDecision::Respawn { reason } => {
                // agent died (or its process died — bd-1np) → respawn with
                // spawn-time kind, reusing the surviving pane.
                match start_agent_with_retry_dyn(&*backend, name, kind, pane_id, &[]) {
                    Ok(a) => recovered.push(json!({
                        "name": name, "pane_id": a.pane_id, "action": "respawned",
                        "kind": kind, "reason": reason
                    })),
                    Err(e) => eprintln!("flywheel: recover {name} failed: {e:#}"),
                }
            }
        }
    }
    print_json(&json!({
        "project": project,
        "healthy": healthy,
        "recovered": recovered.len(),
        "details": recovered,
    }));
    Ok(())
}

/// bd-fw-name-binding-loss-7rc: restore a lost name binding from the spawn
/// registry. Used by `__send` when a target name is not live: if the registry
/// records a pane for that name and the pane still exists (agent restarted →
/// name:null), rename it back and return true so the caller re-resolves.
fn rebind_from_registry(
    backend: &dyn backend::MultiplexerBackend,
    project: &str,
    name: &str,
    ws_id: &str,
) -> Result<bool> {
    let entries = state::load_spawn_registry(project);
    let Some(entry) = registry_entry_for_token(&entries, project, name) else {
        return Ok(false);
    };
    let pane_id = &entry.pane_id;
    let agents = backend.agent_list()?;
    let Some(a) = agents
        .iter()
        .find(|a| a.pane_id == *pane_id && a.workspace_id == ws_id)
    else {
        return Ok(false); // pane gone (respawned) — normal resolution finds the new pane
    };
    if a.name.as_deref() == Some(name) {
        return Ok(true);
    }
    backend.agent_rename(pane_id, Some(name))?;
    Ok(true)
}

// ---- send ----

#[allow(clippy::too_many_arguments)] // clap command handler: one param per CLI flag.
/// If `to` contains a bare kind short (`oc`/`pi`/`gk`) that matches MORE than
/// one agent, return the matched names. A bare short broadens to the whole
/// crew (`oc` → oc, oc2, oc3) — one prompt sent to all of them races the
/// same work (bd-fw-send-bare-short-group). Numbered shorts (`oc2`) and
/// concrete names match exactly one agent and never trigger this.
fn bare_short_group_matches(project: &str, to: &str, names: &[String]) -> Option<Vec<String>> {
    if !to
        .split(',')
        .any(|t| to_want_token(project, t.trim()).broaden)
    {
        return None;
    }
    let matched: Vec<String> = names
        .iter()
        .filter(|n| {
            to.split(',').any(|t| {
                let w = to_want_token(project, t.trim());
                name_matches_want(n, &w)
            })
        })
        .cloned()
        .collect();
    if matched.len() > 1 {
        Some(matched)
    } else {
        None
    }
}

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
    respawn_on_stall: bool,
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
    let mut targets = resolve_agents(&mut c, &ws_id, project, to);
    if targets.is_err() {
        // bd-fw-name-binding-loss-7rc auto-heal: an agent that restarted in
        // its pane lost its name binding — restore from the spawn registry
        // (best-effort) and re-resolve before reporting the miss.
        let backend = backend::multiplexer_backend(cfg)?;
        for tok in to.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let _ = rebind_from_registry(&*backend, project, tok, &ws_id);
        }
        // bd-a2bw: also rebind every nameless pane in the workspace — a
        // `--to coder3` miss must not depend on the token already being
        // the full herdr name.
        if let Ok(live) = c.agent_list() {
            let entries = state::load_spawn_registry(project);
            let mut rebound = false;
            for a in live
                .iter()
                .filter(|a| a.workspace_id == ws_id && a.name.is_none())
            {
                if let Some(e) = entries.iter().find(|e| e.pane_id == a.pane_id) {
                    if c.agent_rename(&a.pane_id, &e.name).is_ok() {
                        rebound = true;
                    }
                }
            }
            // bd-bug-omp-herdr-wedge: the omp herdr integration re-reports the
            // agent as UNNAMED on every processed prompt, so a name-addressed
            // prompt racing the report misses herdr's name index. Settle a
            // beat after the rename before re-resolving.
            if rebound {
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        targets = resolve_agents(&mut c, &ws_id, project, to);
    }
    let targets = targets?;

    // bd-fw-send-bare-short-group: a bare short reaching the whole crew would
    // race one prompt across N agents (observed: 3 agents claimed the same
    // bead, duplicate commits). Refuse with guidance unless --force.
    if !force {
        let names: Vec<String> = targets
            .iter()
            .map(|a| agent_target(a).to_string())
            .collect();
        if let Some(group) = bare_short_group_matches(project, to, &names) {
            bail!(
                "--to {to} matches {} agents ({}): a bare short reaches the whole crew and would race one prompt across all of them. Use an exact name or numbered short (e.g. flywheel-{}-oc, oc2) to target one agent, or pass --force to broadcast deliberately.",
                group.len(),
                group.join(", "),
                project_base(project)
            );
        }
    }

    let timeout = timeout_ms.or_else(|| wait.then(|| cfg.timeout_ms()));
    let mut results: Vec<Value> = Vec::new();
    for a in &targets {
        // --wait means "settle": idle, done, or blocked. An empty `until` array
        // was sent before, which herdr cannot wait on meaningfully.
        let wait_opts = wait.then(|| WaitOptions {
            timeout_ms: timeout,
            until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
        });
        // bd-rdy: stall auto-recovery on the interactive send --wait path. The
        // durable loop escalates stalls (respawn → requeue, bd-send-stall-
        // escalation); the CLI counterpart is one retry after a short delay,
        // then either a same-name respawn (--respawn-on-stall) or an
        // actionable error naming the recovery commands.
        let mut target = agent_target(a).to_string();
        let name = a.name.clone().unwrap_or_else(|| target.clone());
        let mut stalls_seen = 0usize;
        // bd-4pxy: a still-booting agent rejects name-addressed prompts with
        // `agent_not_ready`; herdr accepts pane ids, so retry once via the
        // pane before giving up (bounded — never an infinite loop).
        let mut pane_retried = false;
        let agent = loop {
            match c.agent_prompt(&target, &text, wait_opts.clone()) {
                Ok(agent) => break agent,
                Err(e) if is_agent_not_ready(&e.code) => {
                    match not_ready_retry_target(&target, a, pane_retried) {
                        Some(pane) => {
                            pane_retried = true;
                            eprintln!(
                                "flywheel: {target} agent_not_ready — retrying via pane id {pane}"
                            );
                            target = pane;
                        }
                        None => return Err(e.into()),
                    }
                }
                // bd-bug-omp-herdr-wedge: omp's herdr integration re-reports
                // the agent UNNAMED on each processed prompt, so a
                // name-addressed prompt can miss herdr's name index even
                // after the send-path heal. Rebind from the spawn registry
                // and retry once via the pane.
                Err(e)
                    if matches!(&e.code, BackendErrorCode::Other(c) if c == "agent_not_found")
                        && !pane_retried
                        && !target.contains(':') =>
                {
                    let entry = state::load_spawn_registry(project)
                        .iter()
                        .find(|en| en.name == name)
                        .cloned();
                    match entry {
                        Some(en) => {
                            let _ = c.agent_rename(&en.pane_id, &name);
                            std::thread::sleep(Duration::from_millis(400));
                            pane_retried = true;
                            eprintln!(
                                "flywheel: {target} agent_not_found — rebound from registry, retrying via {}",
                                en.pane_id
                            );
                            target = en.pane_id;
                        }
                        None => return Err(e.into()),
                    }
                }
                Err(e) => match stall_step(&e.code, stalls_seen, respawn_on_stall) {
                    None => return Err(e.into()),
                    Some(StallStep::Retry) => {
                        eprintln!(
                            "flywheel: {target} agent_prompt_stalled — retrying once in {STALL_RETRY_DELAY_SECS}s"
                        );
                        std::thread::sleep(Duration::from_secs(STALL_RETRY_DELAY_SECS));
                        stalls_seen += 1;
                    }
                    Some(StallStep::Respawn) => {
                        let kinds = spawn_kinds_from_log(project);
                        let kind = kinds.get(&name).map(String::as_str).unwrap_or("opencode");
                        eprintln!(
                            "flywheel: {target} stalled again — respawning ({kind}) and re-sending (--respawn-on-stall)"
                        );
                        cmd_internal_respawn(cfg, project, &name, kind, true)?;
                        stalls_seen += 1;
                    }
                    Some(StallStep::GiveUp) => {
                        return Err(stall_giveup_error(&target, project));
                    }
                },
            }
        };
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

// ---- send --wait stall recovery (bd-rdy) ----

/// Delay before the single automatic retry after `agent_prompt_stalled`.
const STALL_RETRY_DELAY_SECS: u64 = 10;

/// One step of the send --wait stall-recovery ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StallStep {
    /// First stall: wait, then re-send once.
    Retry,
    /// Second stall with --respawn-on-stall: respawn under the same name,
    /// then re-send.
    Respawn,
    /// Exhausted: fail with actionable recovery guidance.
    GiveUp,
}

/// bd-rdy: pure stall-classification for the interactive send --wait path.
/// `stalls_seen` = stalls already consumed (0 = first stall). Returns None
/// for non-stall codes so the caller propagates the backend error unchanged.
fn stall_step(
    code: &BackendErrorCode,
    stalls_seen: usize,
    respawn_on_stall: bool,
) -> Option<StallStep> {
    if *code != BackendErrorCode::AgentPromptStalled {
        return None;
    }
    match stalls_seen {
        0 => Some(StallStep::Retry),
        1 if respawn_on_stall => Some(StallStep::Respawn),
        _ => Some(StallStep::GiveUp),
    }
}

/// bd-rdy: the give-up error for a twice-stalled send — names the recovery
/// commands instead of silently dropping the prompt.
fn stall_giveup_error(target: &str, project: &str) -> anyhow::Error {
    anyhow!(
        "agent {target} produced no observed state change twice (agent_prompt_stalled) — the agent is not responding. {}\nAlternatively re-run with --respawn-on-stall to respawn {target} under the same name and re-send automatically.",
        stall_recovery_hint(project),
    )
}

/// bd-rdy: actionable recovery options for a stalled swarm.
fn stall_recovery_hint(project: &str) -> String {
    format!(
        "Recovery options:\n  flywheel recover {project}   respawn dead agents from the spawn registry\n  flywheel kill {project} --force   hard-reset the swarm"
    )
}

/// bd-4pxy: herdr reports `agent_not_ready` when a still-booting agent
/// rejects a prompt addressed by NAME. herdr accepts pane ids too, and a
/// booting agent answers its pane even before the name binding settles —
/// so the send path retries once via the pane id. `None` when the pane-id
/// retry already happened (bounded — never an infinite retry loop).
fn not_ready_retry_target(
    name_used: &str,
    agent: &AgentInfo,
    pane_retried: bool,
) -> Option<String> {
    if pane_retried || name_used == agent.pane_id {
        None
    } else {
        Some(agent.pane_id.clone())
    }
}

/// bd-4pxy: herdr's `agent_not_ready` protocol code arrives unmapped
/// (BackendErrorCode::Other). Everything else (stalls, timeouts, connect)
/// keeps its existing handling.
fn is_agent_not_ready(code: &BackendErrorCode) -> bool {
    matches!(code, BackendErrorCode::Other(c) if c == "agent_not_ready")
}

/// bd-4pxy: a launch_pending agent is NOT idle — it is still booting
/// (status surfaces must not report it as idle).
fn status_label(launch_pending: bool, status: &str) -> String {
    if launch_pending {
        "launching".to_string()
    } else {
        status.to_string()
    }
}

/// bd-4pxy: the spawn-readiness failure — names the `launch_pending` field
/// and every agent still pending. `None` when all agents are launch-ready.
fn launch_pending_error(agents: &[AgentInfo]) -> Option<String> {
    let pending: Vec<&AgentInfo> = agents.iter().filter(|a| is_still_booting(a)).collect();
    if pending.is_empty() {
        return None;
    }
    let list = pending
        .iter()
        .map(|a| {
            format!(
                "{} ({})",
                a.name.as_deref().unwrap_or("<unnamed>"),
                a.pane_id
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "swarm not ready: launch_pending=true for {} — agent(s) still booting; wait and re-run, or check the agent logs (a harness that cannot start stays launch_pending)",
        list
    ))
}

#[cfg(test)]
mod bd_4pxy_tests {
    use super::*;

    fn agent(
        kind: Option<&str>,
        name: Option<&str>,
        ws: &str,
        pending: bool,
        status: AgentStatus,
    ) -> AgentInfo {
        AgentInfo {
            agent: kind.map(String::from),
            agent_status: status,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: pending,
            name: name.map(String::from),
            pane_id: format!("{ws}:p1"),
            revision: 0,
            state_change_seq: 0,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: ws.to_string(),
        }
    }

    #[test]
    fn is_agent_not_ready_matches_protocol_code() {
        assert!(is_agent_not_ready(&BackendErrorCode::Other(
            "agent_not_ready".into()
        )));
        assert!(!is_agent_not_ready(&BackendErrorCode::AgentPromptStalled));
        assert!(!is_agent_not_ready(&BackendErrorCode::Other(
            "agent_prompt_stalled".into()
        )));
        assert!(!is_agent_not_ready(&BackendErrorCode::Timeout));
    }

    #[test]
    fn not_ready_retry_target_pane_once() {
        let a = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        // first not-ready on the NAME → retry via the pane id
        assert_eq!(
            not_ready_retry_target("flywheel-flywheel-coder", &a, false),
            Some("w1:p1".to_string())
        );
        // pane-id retry already consumed → bounded, no third attempt
        assert_eq!(not_ready_retry_target("w1:p1", &a, true), None);
        // an unnamed agent already prompts by pane id → no retry
        let unnamed = agent(Some("omp"), None, "w1", false, AgentStatus::Idle);
        assert_eq!(not_ready_retry_target("w1:p1", &unnamed, false), None);
    }

    #[test]
    fn status_label_launch_pending_is_not_idle() {
        assert_eq!(status_label(true, AgentStatus::Idle.as_str()), "launching");
        assert_eq!(status_label(false, AgentStatus::Idle.as_str()), "idle");
        assert_eq!(
            status_label(false, AgentStatus::Working.as_str()),
            "working"
        );
    }

    #[test]
    fn launch_pending_error_names_field_and_agent() {
        // bd-a2bw: a truly-booting agent is launch_pending AND not yet
        // interactive_ready. Idle + ready with launch_pending set is stale
        // (covered by is_still_booting_ignores_stale_launch_pending).
        let mut pending = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            true,
            AgentStatus::Unknown,
        );
        pending.interactive_ready = false;
        let ready = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let msg = launch_pending_error(&[pending, ready]).expect("pending agent names the error");
        assert!(msg.contains("launch_pending"), "{msg}");
        assert!(msg.contains("flywheel-flywheel-coder"), "{msg}");
        assert!(msg.contains("w1:p1"), "{msg}");
        let all_ready = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        assert!(launch_pending_error(&[all_ready]).is_none());
    }

    #[test]
    fn want_token_omp1_carries_kind_and_ordinal() {
        let w = to_want_token("flywheel", "omp1");
        assert_eq!(w.short.as_deref(), Some("omp"));
        assert_eq!(w.ordinal, 1);
        assert_eq!(w.name, "flywheel-flywheel-omp");
        let w = to_want_token("flywheel", "omp2");
        assert_eq!(w.short.as_deref(), Some("omp"));
        assert_eq!(w.ordinal, 2);
        let w = to_want_token("flywheel", "flywheel-flywheel-coder");
        assert!(w.short.is_none(), "concrete name carries no kind short");
        let w = to_want_token("flywheel", "w1:p1");
        assert!(w.short.is_none(), "pane id carries no kind short");
    }

    #[test]
    fn match_indices_kind_fallback_resolves_omp1() {
        // the bd-4pxy witness shape: an omp agent NAMED flywheel-<base>-coder
        let omp1 = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let omp2 = agent(
            Some("omp"),
            Some("flywheel-flywheel-doer"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let oc = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let agents = vec![omp1, omp2, oc];
        // --to omp1: name flywheel-flywheel-omp does not exist → first kind=omp
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "omp1")]),
            vec![0]
        );
        // --to omp2 → second kind=omp
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "omp2")]),
            vec![1]
        );
        // bare --to omp → first kind=omp (no flywheel-flywheel-omp name)
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "omp")]),
            vec![0]
        );
    }

    #[test]
    fn match_indices_name_match_beats_kind_fallback() {
        let oc = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let omp = agent(
            Some("omp"),
            Some("flywheel-flywheel-coder"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let agents = vec![oc, omp];
        // --to oc1 resolves by NAME (exists) — kind fallback must not add a
        // second, differently-named kind=oc agent
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "oc1")]),
            vec![0]
        );
    }

    #[test]
    fn match_indices_broaden_keeps_whole_crew() {
        let oc = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let oc2 = agent(
            Some("oc"),
            Some("flywheel-flywheel-oc2"),
            "w1",
            false,
            AgentStatus::Idle,
        );
        let agents = vec![oc, oc2];
        assert_eq!(
            match_indices(&agents, &[to_want_token("flywheel", "oc")]),
            vec![0, 1]
        );
    }

    fn agent_on_pane(
        kind: Option<&str>,
        name: Option<&str>,
        pane_id: &str,
        pending: bool,
        status: AgentStatus,
        ready: bool,
    ) -> AgentInfo {
        let mut a = agent(kind, name, "w70", pending, status);
        a.pane_id = pane_id.to_string();
        a.interactive_ready = ready;
        a
    }

    /// bd-a2bw: `--to w70:p3` must hit the pane even when herdr name is null
    /// (omp profile split; ADR-0018 seq-1 suffix-less names never land as
    /// the herdr name on later splits).
    #[test]
    fn match_indices_resolves_pane_id_when_name_null() {
        let p1 = agent_on_pane(
            Some("omp"),
            Some("flywheel-toron-coder"),
            "w70:p1",
            false,
            AgentStatus::Idle,
            true,
        );
        let p3 = agent_on_pane(
            Some("omp"),
            None,
            "w70:p3",
            false,
            AgentStatus::Working,
            true,
        );
        let agents = vec![p1, p3];
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "w70:p3")]),
            vec![1],
            "--to pane id must match AgentInfo.pane_id when name is null"
        );
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "w70:p1")]),
            vec![0]
        );
    }

    /// bd-a2bw: `--to coder1` is suffix-less (ADR-0018); `--to coder3`
    /// resolves after overlaying the spawn-registry name onto a nameless
    /// omp pane (herdr kind stays `omp`, so kind fallback cannot save us).
    #[test]
    fn overlay_registry_names_lets_coder_short_hit_nameless_omp() {
        assert_eq!(
            to_want_token("toron", "coder1").name,
            "flywheel-toron-coder"
        );
        assert_eq!(
            to_want_token("toron", "coder3").name,
            "flywheel-toron-coder3"
        );
        let mut agents = vec![
            agent_on_pane(
                Some("omp"),
                Some("flywheel-toron-coder"),
                "w70:p1",
                false,
                AgentStatus::Idle,
                true,
            ),
            agent_on_pane(Some("omp"), None, "w70:p3", false, AgentStatus::Idle, true),
        ];
        let entries = vec![
            state::SpawnEntry::profiled(
                "w70:p1",
                "flywheel-toron-coder",
                "coder",
                "omp",
                Some("flywheel-toron-coder".into()),
            ),
            state::SpawnEntry::profiled(
                "w70:p3",
                "flywheel-toron-coder3",
                "coder",
                "omp",
                Some("flywheel-toron-coder3".into()),
            ),
        ];
        overlay_registry_names(&mut agents, &entries);
        assert_eq!(agents[1].name.as_deref(), Some("flywheel-toron-coder3"));
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "coder3")]),
            vec![1]
        );
        assert_eq!(
            match_indices(&agents, &[to_want_token("toron", "coder1")]),
            vec![0]
        );
        assert_eq!(
            registry_entry_for_token(&entries, "toron", "w70:p3").map(|e| e.name.as_str()),
            Some("flywheel-toron-coder3")
        );
        assert_eq!(
            registry_entry_for_token(&entries, "toron", "coder3").map(|e| e.name.as_str()),
            Some("flywheel-toron-coder3")
        );
    }

    /// bd-a2bw: omp at `π >` with a session is idle + interactive_ready;
    /// herdr may still report launch_pending. That bit is stale — send
    /// must proceed.
    #[test]
    fn is_still_booting_ignores_stale_launch_pending() {
        let stale = agent_on_pane(Some("omp"), None, "w70:p2", true, AgentStatus::Idle, true);
        assert!(
            !is_still_booting(&stale),
            "idle + interactive_ready is not booting even if launch_pending"
        );
        let booting = agent_on_pane(
            Some("omp"),
            None,
            "w70:p2",
            true,
            AgentStatus::Unknown,
            false,
        );
        assert!(is_still_booting(&booting));
        assert!(!is_still_booting(&agent_on_pane(
            Some("omp"),
            Some("flywheel-toron-coder"),
            "w70:p1",
            false,
            AgentStatus::Idle,
            true,
        )));
        assert!(launch_pending_error(&[stale]).is_none());
        assert!(launch_pending_error(&[booting]).is_some());
    }

    #[test]
    fn recover_probe_kind_prefers_registry_harness() {
        let entry = state::SpawnEntry::profiled(
            "w70:p1",
            "flywheel-toron-coder",
            "coder",
            "omp",
            Some("flywheel-toron-coder".into()),
        );
        let empty = std::collections::HashMap::new();
        assert_eq!(recover_probe_kind(&entry, &empty), "omp");
        let mut log = std::collections::HashMap::new();
        log.insert("flywheel-toron-coder".into(), "opencode".into());
        assert_eq!(
            recover_probe_kind(&entry, &log),
            "omp",
            "registry harness beats a stale spawn-log kind"
        );
        let legacy = state::SpawnEntry::legacy("w70:p1", "flywheel-toron-oc");
        assert_eq!(recover_probe_kind(&legacy, &empty), "opencode");
        log.insert("flywheel-toron-oc".into(), "pi".into());
        assert_eq!(recover_probe_kind(&legacy, &log), "pi");
    }
}

#[cfg(test)]
mod send_stall_recovery_tests {
    use super::*;

    #[test]
    fn first_stall_always_retries() {
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 0, false),
            Some(StallStep::Retry)
        );
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 0, true),
            Some(StallStep::Retry)
        );
    }

    #[test]
    fn second_stall_respawns_only_with_flag() {
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 1, true),
            Some(StallStep::Respawn)
        );
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 1, false),
            Some(StallStep::GiveUp)
        );
    }

    #[test]
    fn exhausted_stall_always_gives_up() {
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 2, true),
            Some(StallStep::GiveUp)
        );
        assert_eq!(
            stall_step(&BackendErrorCode::AgentPromptStalled, 2, false),
            Some(StallStep::GiveUp)
        );
    }

    #[test]
    fn non_stall_errors_propagate() {
        assert_eq!(stall_step(&BackendErrorCode::Timeout, 0, false), None);
        assert_eq!(
            stall_step(&BackendErrorCode::Other("agent_not_found".into()), 1, true),
            None
        );
    }

    #[test]
    fn giveup_error_names_recovery_commands() {
        let msg = stall_giveup_error("flywheel-demo-oc", "demo").to_string();
        assert!(msg.contains("agent_prompt_stalled"), "{msg}");
        assert!(msg.contains("flywheel recover demo"), "{msg}");
        assert!(msg.contains("flywheel kill demo --force"), "{msg}");
        assert!(msg.contains("--respawn-on-stall"), "{msg}");
    }
}

// ---- interrupt / view / watch / zoom / attach / kill ----

/// Kill-style key names that terminate the agent process (not just cancel
/// the current call). Guarded behind --force (bd-fw-interrupt-kills-agent-or3).
const KILL_KEYS: [&str; 4] = ["ctrl+c", "ctrl-c", "^c", "C-c"];

/// The first kill-style key in the set, if any (pure — unit-tested).
fn kill_key<'a>(keys: &[&'a str]) -> Option<&'a str> {
    keys.iter().find(|k| KILL_KEYS.contains(k)).copied()
}

fn cmd_interrupt(
    cfg: &Config,
    project: &str,
    to: &str,
    keys: &[String],
    force: bool,
) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;
    let keys: Vec<&str> = if keys.is_empty() {
        vec!["Escape"]
    } else {
        keys.iter().map(|s| s.as_str()).collect()
    };
    if !force && let Some(k) = kill_key(&keys) {
        bail!(
            "--keys {k} TERMINATES the agent process (pane drops to a shell and the session binding is lost). Prefer the default Escape, which cancels the current tool call and returns to the prompt. Pass --force to send {k} deliberately."
        );
    }
    let is_kill = kill_key(&keys).is_some();
    for a in &targets {
        c.agent_send_keys(agent_target(a), &keys)?;
        if std::io::stdout().is_terminal() {
            println!(
                "interrupted {} ({})",
                dim(agent_target(a)),
                if is_kill { "kill keys" } else { "soft" }
            );
        }
    }
    log(
        project,
        EventKind::Interrupt,
        json!({ "to": to, "keys": keys, "kill": is_kill }),
    );
    Ok(())
}

/// bd-fw-view-alt-screen-blocked-ct9: a working agent's `recent` history is
/// on the alternate screen and cannot be captured (herdr rejects with
/// `agent_not_idle`). Fall back to the `visible` buffer with a note instead
/// of failing the whole view. Never falls back when the caller explicitly
/// asked for `visible`.
///
/// bd-3hd: every read (whatever source — recent, visible, recent-unwrapped)
/// is passed through `sanitize_pane_text` so alternate-screen TUI chrome
/// (ANSI/OSC sequences + box-drawing borders) never reaches the terminal.
fn client_read_with_fallback(
    c: &mut Client,
    target: &str,
    source: &str,
    lines: Option<u32>,
) -> Result<String> {
    let mut ops = herdr_shim::ClientAltScreenRead::new(c, target, lines);
    let raw = alt_screen_read_ladder(&mut ops, source)
        .map_err(|e| anyhow!("{target}: alt-screen read failed: {e:#}"))?;
    Ok(sanitize_pane_text(&raw))
}

fn cmd_view(cfg: &Config, project: &str, to: &str, lines: Option<u32>, source: &str) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let targets = resolve_agents(&mut c, &ws_id, project, to)?;
    for a in &targets {
        let text = client_read_with_fallback(&mut c, agent_target(a), source, lines)?;
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

/// `flywheel output`: read an agent's session from the opencode structured
/// store (bd-eign.5). Logic lives in herdr_shim::read_session_log; this is a
/// thin runtime + render stub.
fn cmd_output(
    cfg: &Config,
    project: &str,
    role: Option<&str>,
    session: Option<&str>,
    limit: u32,
) -> Result<()> {
    let dir = project_dir_str(cfg, project)?;
    let log = herdr_shim::read_session_log(&dir, role, session, limit)?;
    print_json(&log);
    Ok(())
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
        if let Some(u) = until_status
            && st == u
            && all_matching(&mut c, &watch_names, u)?
        {
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

/// Names of agents still in `working`. Kill must not reap these unless
/// `--force-working` — cargo test routinely exceeds 15m.
fn working_agent_names(agents: &[AgentInfo]) -> Vec<String> {
    agents
        .iter()
        .filter(|a| a.agent_status == herdr_shim::AgentStatus::Working)
        .map(|a| a.name.clone().unwrap_or_else(|| a.pane_id.clone()))
        .collect()
}

fn cmd_kill(cfg: &Config, project: &str, force: bool, force_working: bool) -> Result<()> {
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
    let working = working_agent_names(&agents);
    if !working.is_empty() && !force_working {
        bail!(
            "refuse kill: {} agent(s) still working ({}). Long jobs (cargo test) often exceed 15m. Pass --force-working to reap anyway.",
            working.len(),
            working.join(", ")
        );
    }
    c.workspace_close(&ws_id)?;
    // tidy-up: drop the project's agent state files (stale context signals
    // must not survive into the next spawn — freshness only protects so much)
    let _ = std::fs::remove_dir_all(agentstate::project_state_dir(&project_dir(cfg, project)));
    // clear the identity files for the dead panes (best-effort — a missing
    // file is a no-op, so double-exit is harmless)
    if let Ok(mail) = backend::mail_backend(cfg)
        && let Some(key) = project_dir(cfg, project).to_str()
    {
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
        // explicit targets: herdr agent names resolve through the spawn
        // registry (bd-627z.4 — the pinned pane→identity binding, so the
        // same agent gets the same identity every call); already-valid
        // identities pass through unchanged (bd-odc)
        to.split(',')
            .map(|s| resolve_mail_target_registry(project, s.trim()))
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
    // bd-627z.2: first-contact auto-register backstop. If the daemon was down
    // at spawn, the agents have no identities yet; assign auto-registers each
    // resolved target on first use (idempotent upsert) so lane assignment
    // never needs manual iterative registration. Best-effort: a registration
    // failure (daemon still down) keeps the send attempt + actionable error.
    for t in &targets {
        if let Err(e) =
            mail.register_agent(&key, t, "opencode", "opencode", "flywheel mail CLI actor")
        {
            eprintln!("flywheel: assign auto-register of {t} failed (continuing): {e:#}");
        }
    }
    if let Err(e) = mail.send(&key, &targets, &subject, &body, "high", true, None) {
        // bd-627z.2: actionable errors — never a bare "unknown recipient".
        let msg = format!("{e:#}");
        if msg.contains("unknown recipient") {
            bail!(
                "assign failed: {msg} — auto-registration ran for each target; if it \
                 could not register, the toron mail daemon may be down (start it: toron \
                 serve) then retry the assign"
            );
        }
        return Err(e);
    }
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
         When finished, reply a concise summary in mail thread `{}` \
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
            unread,
            limit,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            // 2026-08-15: an UNREGISTERED --as identity previously made the
            // inbox projection scan without an agent scope (multi-second
            // hang). Fail fast with the registration command instead.
            if mail.whois(&key, &name).is_err() {
                anyhow::bail!(
                    "mail identity {name} is not registered for {project} — register it \
                     first: flywheel mail register {project} --name <adjnoun-pin>"
                );
            }
            // --unread needs the filtered surface; --urgent and the plain
            // path share the legacy inbox() call (toron's in-process fetch
            // ignores urgent_only, the filter applies it in inbox_filtered).
            let v = if *unread {
                mail.inbox_filtered(
                    &key,
                    &name,
                    &mail_shim::MailInboxFilter {
                        unread_only: true,
                        include_bodies: true,
                        ..Default::default()
                    },
                )?
            } else {
                mail.inbox(&key, &name, *urgent, *limit)?
            };
            print_json(&v);
        }
        MailCmd::Reply {
            project,
            message_id,
            body,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let v = mail.reply(&key, message_id, body, None)?;
            print_json(&v);
        }
        MailCmd::Read {
            project,
            message_id,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.mark_read(&key, &name, message_id)?;
            print_json(&v);
        }
        MailCmd::Ack {
            project,
            message_id,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.ack(&key, &name, message_id)?;
            print_json(&v);
        }
        MailCmd::Search {
            project,
            query,
            limit,
            sender,
            importance,
            thread_id,
            recency,
            bodies,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.search(
                &key,
                query,
                *limit,
                &mail_shim::MailSearchFilter {
                    sender: sender.clone(),
                    importance: importance.clone(),
                    thread_id: thread_id.clone(),
                    ranking: Some(if *recency { "recency" } else { "relevance" }.to_string()),
                    include_body_md: *bodies,
                    ..Default::default()
                },
            )?;
            print_json(&v);
        }
        MailCmd::Summarize {
            project,
            thread_id,
            examples,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.summarize_thread(&key, thread_id, *examples)?;
            print_json(&v);
        }
        MailCmd::Whois { project, agent } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let v = mail.whois(&key, agent)?;
            print_json(&v);
        }
        MailCmd::Register {
            project,
            name,
            program,
            model,
            task,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let name = name.clone().unwrap_or_default();
            let v = mail.register_agent(
                &key,
                &name,
                program,
                model,
                task.as_deref().unwrap_or("flywheel mail CLI actor"),
            )?;
            print_json(&v);
        }
        MailCmd::Renew {
            project,
            extend_seconds,
            paths,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.renew(&key, &name, *extend_seconds, Some(paths), None)?;
            print_json(&v);
        }
        MailCmd::ForceUnlock {
            project,
            path,
            holder,
            note,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.force_release_file_reservation(
                &key,
                &name,
                path,
                holder.as_deref(),
                note.as_deref(),
                false,
            )?;
            print_json(&v);
        }
        MailCmd::Slot { cmd } => match cmd {
            MailSlotCmd::Acquire {
                project,
                slot,
                exclusive,
                ttl_seconds,
                as_agent,
            } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                let v = mail.acquire_slot(&key, &name, slot, *exclusive, *ttl_seconds)?;
                print_json(&v);
            }
            MailSlotCmd::Renew {
                project,
                slot,
                extend_seconds,
                as_agent,
            } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                let v = mail.renew_slot(&key, &name, slot, *extend_seconds)?;
                print_json(&v);
            }
            MailSlotCmd::Release {
                project,
                slot,
                as_agent,
            } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                let v = mail.release_slot(&key, &name, slot)?;
                print_json(&v);
            }
        },
        MailCmd::Policy {
            project,
            policy,
            as_agent,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_as(cfg, &key, as_agent.as_deref())?;
            mail.ensure_project(&key)?;
            let name = mail.name.clone();
            let v = mail.set_contact_policy(&key, &name, policy)?;
            print_json(&v);
        }
        MailCmd::Digest {
            project,
            as_agent,
            all,
        } => {
            let key = project_dir_str(cfg, project)?;
            if *all {
                let mail = mail_or_die(cfg)?;
                mail.ensure_project(&key)?;
                let agents = mail.list_agents(&key, None)?;
                let mut out = Vec::new();
                if let Some(arr) = agents.as_array() {
                    for a in arr {
                        let n = a.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        if let Ok(d) = digest_for(&mail, &key, n) {
                            out.push(d);
                        }
                    }
                }
                print_json(&json!(out));
            } else {
                let mail = mail_as(cfg, &key, as_agent.as_deref())?;
                mail.ensure_project(&key)?;
                let name = mail.name.clone();
                print_json(&digest_for(&mail, &key, &name)?);
            }
        }
        MailCmd::Watch {
            project,
            thread_id,
            as_agent,
            timeout_secs,
            interval_secs,
        } => {
            cmd_mail_watch(
                cfg,
                project,
                thread_id,
                as_agent.as_deref(),
                *timeout_secs,
                *interval_secs,
            )?;
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
        MailCmd::Locks { cmd } => match cmd {
            MailLockCmd::Reserve {
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
            MailLockCmd::List { project, holder } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_or_die(cfg)?;
                let v = mail.list_active(&key, holder.as_deref(), None)?;
                print_json(&v);
            }
            MailLockCmd::Show { project, path } => {
                let key = project_dir_str(cfg, project)?;
                let mail = mail_or_die(cfg)?;
                let v = mail.list_active(&key, None, Some(path))?;
                print_json(&v);
            }
        },
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
        MailCmd::Agents { project, all } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            // Roster = CURRENT agents by default (active <24h); --all shows
            // the full registration history (ADR-0018 keeps legacy
            // identities registered for attribution).
            let active_within = if *all { None } else { Some(1) };
            let v = mail.list_agents(&key, active_within)?;
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

// ---- mail digest / watch / collab (real-time collab primitives) ----

/// Per-agent digest from the live mailbox (mirror of the `__mail-digest`
/// leaf, surfaced for the orchestrator). Reads `sender`/`from` aliases and
/// counts high|urgent as urgent.
///
/// NOTE on `unread`: toron's projection has NO read-state — `unread_only`
/// is a documented no-op (messages are "fresh on first read" per pipeline
/// dedup). The digest therefore reports the RECENT inbox (limit 10) with an
/// honest label: `recent` = messages in the projection window, `urgent` =
/// the [`wake_urgent`]-filtered high|urgent count among them (S8.2: read or
/// already-executed assigns do not wake). The wake trigger uses
/// `urgent > 0`, which IS meaningful (importance is a real message
/// attribute).
fn digest_for(mail: &mail_shim::Mail, key: &str, name: &str) -> Result<Value> {
    let v = mail.inbox_filtered(
        key,
        name,
        &mail_shim::MailInboxFilter {
            unread_only: false,
            include_bodies: true,
            limit: 10,
            ..Default::default()
        },
    )?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let recent = arr.len();
    let urgent_rows = wake_urgent(&arr);
    let urgent = urgent_rows.len();
    let last = arr.first();
    // wake key: the most recent URGENT message that survives the wake
    // filter (the loop wakes once per id)
    let last_urgent = urgent_rows.first().copied().or(last);
    let last_from = last.and_then(|m| {
        m.get("sender")
            .or_else(|| m.get("from"))
            .and_then(|f| f.as_str())
    });
    Ok(json!({
        "agent": name,
        // the caller's mail is bound to this identity — wake fetches as it
        "mail_identity": name,
        "recent": recent,
        "urgent": urgent,
        "last_id": last_urgent.and_then(|m| {
            m.get("id").or_else(|| m.get("message_id")).and_then(|i| i.as_str())
        }),
        "last_from": last_from,
        "last_subject": last.and_then(|m| m.get("subject").and_then(|s| s.as_str())),
        "thread_id": last.and_then(|m| m.get("thread_id").and_then(|t| t.as_str())),
    }))
}

/// S8.1: the wake identity for a herdr agent name. The spawn-registry /
/// agent-names pin (pane → name → mail identity) wins; otherwise the
/// canonical name itself — the identity flywheel registers at spawn and the
/// wake backstop registers idempotently BEFORE instructing fetch. Never the
/// legacy adjective+noun hash: a wake must name an identity that exists (or
/// is about to be registered), or the "fetch as X" instruction 404s
/// (debrief D1/D2 — "fetch as flywheel-toron-oc4" for a never-registered
/// name).
pub(crate) fn wake_identity_pin(
    reg: &[state::SpawnEntry],
    names: &[state::SpawnEntry],
    name: &str,
) -> String {
    for e in reg.iter().chain(names.iter()) {
        if e.name == name {
            if let Some(mid) = e.mail_identity.as_deref().filter(|m| !m.is_empty()) {
                return mid.to_string();
            }
            break;
        }
    }
    name.to_string()
}

/// S8.2: which urgent inbox rows legitimately wake an agent.
///
/// Assign messages (`[assign]` subjects) wake only when UNREAD
/// (`read_ts_us` absent — the agent has not fetched them) AND not-yet-
/// executed (no in-thread reply newer than the assign). Identical
/// (sender, subject, body) assigns within the window collapse to the first
/// — the duplicate-assign double-wake (debrief D1: two identical assigns
/// both woke the agent). All other urgent rows pass through unchanged.
///
/// Pure + deterministic; the digest paths (loop wake + operator `mail
/// digest`) share this one filter.
pub(crate) fn wake_urgent(rows: &[Value]) -> Vec<&Value> {
    let urgent = |m: &Value| {
        m.get("importance")
            .and_then(|i| i.as_str())
            .map(|i| i == "high" || i == "urgent")
            .unwrap_or(false)
    };
    let is_assign = |m: &Value| {
        m.get("subject")
            .and_then(|s| s.as_str())
            .map(|s| s.starts_with("[assign]"))
            .unwrap_or(false)
    };
    let key_of = |m: &Value| {
        (
            m.get("sender")
                .or_else(|| m.get("from"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            m.get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            m.get("body_md")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        )
    };
    // Pass 1: urgent rows, with identical (sender, subject, body) assigns
    // collapsed to the first (the duplicate-assign double-wake, debrief D1).
    // Duplicate copies are remembered so they never count as thread replies
    // in pass 2.
    let mut seen: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    let mut duplicate_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut deduped: Vec<&Value> = Vec::new();
    for m in rows {
        if !urgent(m) {
            continue;
        }
        if is_assign(m) && !seen.insert(key_of(m)) {
            if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                duplicate_ids.insert(id);
            }
            continue; // duplicate assign — same sender+subject+body
        }
        deduped.push(m);
    }
    // Pass 2: assigns wake only when UNREAD (`read_ts_us` absent) AND
    // not-yet-executed (no in-thread reply newer than the assign, judged
    // against ALL rows minus the deduplicated copies — a duplicate copy
    // never counts as a reply, a real reply (urgent or not) always does).
    let mut out = Vec::new();
    for m in &deduped {
        if !is_assign(m) {
            out.push(*m);
            continue;
        }
        if m.get("read_ts_us").map(|v| !v.is_null()).unwrap_or(false) {
            continue; // already read — the agent saw it
        }
        if thread_has_reply(rows, &duplicate_ids, m) {
            continue; // executed — the thread already moved
        }
        out.push(*m);
    }
    out
}

/// A later, non-duplicate message exists on the same thread as `m` (the
/// thread moved). `duplicate_ids` are the collapsed assign copies — they do
/// not count as replies.
fn thread_has_reply(
    rows: &[Value],
    duplicate_ids: &std::collections::HashSet<&str>,
    m: &Value,
) -> bool {
    let Some(tid) = m.get("thread_id").and_then(|t| t.as_str()) else {
        return false;
    };
    let Some(ts) = m.get("created_ts_us").and_then(|t| t.as_i64()) else {
        return false;
    };
    rows.iter().any(|r| {
        !std::ptr::eq(r, m)
            && !r
                .get("id")
                .and_then(|i| i.as_str())
                .map(|i| duplicate_ids.contains(i))
                .unwrap_or(false)
            && r.get("thread_id").and_then(|t| t.as_str()) == Some(tid)
            && r.get("created_ts_us")
                .and_then(|t| t.as_i64())
                .map(|t| t > ts)
                .unwrap_or(false)
    })
}

/// S8.3/S8.4: the mail profile a spawn registers — the CANONICAL herdr name
/// as the identity (the random npub stays underneath; toron accepts any
/// name, the legacy agent-mail adjective+noun whitelist does not apply),
/// with a fresh task stamp whose idempotent upsert overwrites stale task
/// fields from a prior session (debrief A2: a 2026-08-14 identity still
/// advertised `bd-qk9q`).
pub(crate) fn spawn_agent_profile(name: &str, kind: &str, project: &str) -> backend::AgentProfile {
    backend::AgentProfile {
        name: name.to_string(),
        program: Some(kind.to_string()),
        model: Some(kind.to_string()),
        persona: None,
        task_description: Some(format!("{name} (flywheel agent for {project})")),
    }
}

#[cfg(test)]
mod wake_identity_tests {
    use super::*;
    use crate::state::SpawnEntry;

    fn entry(name: &str, mid: Option<&str>) -> SpawnEntry {
        match mid {
            Some(m) => SpawnEntry::with_mail_identity("p1", name, m),
            None => SpawnEntry::legacy("p1", name),
        }
    }

    fn assign_row(id: &str, body: &str, thread: &str, created_us: i64, read: bool) -> Value {
        json!({
            "id": id,
            "sender": "CoralFinch",
            "subject": "[assign] toron → bd-x.1",
            "body_md": body,
            "importance": "high",
            "thread_id": thread,
            "created_ts_us": created_us,
            "read_ts_us": read.then(|| created_us + 1),
        })
    }

    /// S8.1: the wake identity is the registry pin, else the canonical name
    /// itself — never a not-yet-registered alias.
    #[test]
    fn wake_identity_prefers_pin_else_canonical_name() {
        let reg = vec![entry("flywheel-toron-oc", Some("DustySparrow"))];
        let names = vec![entry("flywheel-toron-oc", Some("DustySparrow"))];
        assert_eq!(
            wake_identity_pin(&reg, &names, "flywheel-toron-oc"),
            "DustySparrow",
            "legacy registry pin still wins"
        );
        assert_eq!(
            wake_identity_pin(&[], &names, "flywheel-toron-oc"),
            "DustySparrow",
            "agent-names mirror pin wins when the registry is empty"
        );
        assert_eq!(
            wake_identity_pin(&[], &[], "flywheel-toron-oc4"),
            "flywheel-toron-oc4",
            "unregistered name resolves to itself — the wake registers it"
        );
        let unpinned = vec![entry("flywheel-toron-oc", None)];
        assert_eq!(
            wake_identity_pin(&unpinned, &[], "flywheel-toron-oc"),
            "flywheel-toron-oc",
            "legacy entry without a pin falls back to the canonical name"
        );
    }

    /// S8.2: a plain urgent message always wakes.
    #[test]
    fn wake_urgent_keeps_normal_urgent() {
        let rows = vec![json!({
            "id": "m1", "sender": "PinkGorge", "subject": "[bd-1] help",
            "body_md": "x", "importance": "urgent", "thread_id": "t1",
            "created_ts_us": 1000,
        })];
        assert_eq!(wake_urgent(&rows).len(), 1);
    }

    /// S8.2: two identical assigns (same sender+subject+body) collapse to one.
    #[test]
    fn wake_urgent_collapses_duplicate_assigns() {
        let rows = vec![
            assign_row("18072be9", "Assigned beads: bd-gsvn.2", "t-a", 1000, false),
            assign_row("4436c7eb", "Assigned beads: bd-gsvn.2", "t-a", 1001, false),
        ];
        let out = wake_urgent(&rows);
        assert_eq!(out.len(), 1, "identical assigns wake once");
        assert_eq!(out[0]["id"], "18072be9", "the first assign wins");
    }

    /// S8.2: an assign the agent already read does not wake.
    #[test]
    fn wake_urgent_skips_read_assign() {
        let rows = vec![assign_row(
            "m1",
            "Assigned beads: bd-x.1",
            "t-1",
            1000,
            true,
        )];
        assert_eq!(wake_urgent(&rows).len(), 0, "read assign must not wake");
    }

    /// S8.2: an assign whose thread already has a reply is executed — no wake.
    #[test]
    fn wake_urgent_skips_executed_assign() {
        let rows = vec![
            assign_row("18072be9", "Assigned beads: bd-gsvn.2", "t-a", 1000, false),
            json!({
                "id": "reply-1", "sender": "YellowCastle",
                "subject": "Re: [assign] toron → bd-gsvn.2",
                "body_md": "all closed", "importance": "normal",
                "thread_id": "t-a", "created_ts_us": 2000,
            }),
        ];
        assert_eq!(
            wake_urgent(&rows).len(),
            0,
            "executed assign (thread moved) must not wake"
        );
    }

    /// S8.2: an unread, unexecuted assign DOES wake.
    #[test]
    fn wake_urgent_keeps_fresh_assign() {
        let rows = vec![assign_row(
            "m1",
            "Assigned beads: bd-x.1",
            "t-1",
            1000,
            false,
        )];
        assert_eq!(wake_urgent(&rows).len(), 1);
    }

    /// S9 (ADR-0018 D1a): the spawn profile registers the adj+noun PIN as
    /// the mail identity with a fresh task stamp (the upsert clears stale
    /// prior-session tasks).
    #[test]
    fn spawn_profile_uses_adjnoun_pin_and_fresh_task() {
        let p = spawn_agent_profile("DustySparrow", "opencode", "toron");
        assert_eq!(p.name, "DustySparrow", "the adj+noun pin is the identity");
        let task = p.task_description.unwrap();
        assert!(task.contains("flywheel agent for toron"), "{task}");
        assert!(
            !task.contains("bd-"),
            "no stale bead id in the fresh stamp: {task}"
        );
    }

    /// S9 (ADR-0018 D1a): a pin draw whose first candidate is taken (spawn
    /// registry pin or registered agent) redraws to a DIFFERENT valid
    /// adj+noun pair; a fully-blocked prefix still terminates.
    #[test]
    fn pin_collision_redraws() {
        let seed = state::fnv1a64("flywheel-toron-oc");
        let first = adjnoun_from_fnv(seed);
        assert!(valid_pair(&first, &ADJECTIVES, &NOUNS).is_some(), "{first}");
        let taken: std::collections::HashSet<String> =
            std::collections::HashSet::from([first.clone()]);
        let redrawn = unique_adjnoun_pin(&taken, seed);
        assert_ne!(redrawn, first, "collision must redraw");
        assert!(valid_pair(&redrawn, &ADJECTIVES, &NOUNS).is_some());
        let blocked: std::collections::HashSet<String> =
            (0..10).map(|i| adjnoun_from_fnv(seed + i)).collect();
        let out = unique_adjnoun_pin(&blocked, seed);
        assert!(!blocked.contains(&out), "redraw escapes the blocked prefix");
    }

    /// S9 (ADR-0018 D1a): the pin lands in the spawn registry AND the
    /// project-local agent-names.json mirror; wake + debate/send resolve the
    /// slug THROUGH the pin (the slug stays the technical pane handle).
    #[test]
    fn spawn_pin_flows_into_agent_names_and_resolves() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-s9-names-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let project = "/tmp/fw-s9-proj";
        let entries = vec![state::SpawnEntry::with_mail_identity(
            "w7:p1",
            "flywheel-toron-oc",
            "DustySparrow",
        )];
        state::save_spawn_registry(project, &entries).expect("save registry");
        state::save_agent_names(std::path::Path::new(project), &entries).expect("save names");
        let names = state::load_agent_names(std::path::Path::new(project));
        assert_eq!(names[0].mail_identity.as_deref(), Some("DustySparrow"));
        assert_eq!(
            wake_identity_pin(
                &state::load_spawn_registry(project),
                &names,
                "flywheel-toron-oc"
            ),
            "DustySparrow",
            "wake reads the pin through the binding"
        );
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-toron-oc"),
            "DustySparrow",
            "debate/send read the pin through the binding"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S9 (ADR-0018 D1a) integration (embedded mailbox): spawn registers a
    /// UNIQUE adj+noun pin — never the slug — keeps it on re-spawn
    /// (idempotent), and redraws when the agents table already holds the
    /// seed-draw candidate.
    #[test]
    fn register_spawned_agent_pins_unique_adjnoun_identity() {
        let _g = crate::config::lock_env();
        let data = std::env::temp_dir().join(format!("fw-s9-mail-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("TORON_DATA_DIR", &data) };
        let cfg = Config::default();
        let project = "/tmp/fw-s9-mail-proj";
        let slug = "flywheel-toron-oc";
        let pin = register_spawned_agent(&cfg, project, slug, "opencode", "w7:p1")
            .expect("first spawn pins an identity");
        assert!(
            valid_pair(&pin, &ADJECTIVES, &NOUNS).is_some(),
            "adj+noun pin: {pin}"
        );
        assert_ne!(pin, slug, "the slug is the pane handle, not the identity");
        let again = register_spawned_agent(&cfg, project, slug, "opencode", "w7:p1")
            .expect("re-spawn keeps the pin");
        assert_eq!(again, pin, "re-spawn must not draw a fresh pin");
        // agents-table collision: find a name whose seed-draw equals the
        // pinned identity, spawn it — its draw must redraw around the taken
        // pin (the 9900-pair space guarantees a colliding candidate).
        let target = pin.clone();
        let collider = (0..200_000u64)
            .map(|i| format!("s9spawn-{i}"))
            .find(|n| adjnoun_from_fnv(state::fnv1a64(n)) == target)
            .expect("a colliding name exists in the 9900-pair space");
        let pin2 = register_spawned_agent(&cfg, project, &collider, "opencode", "w7:p2")
            .expect("colliding spawn draws a fresh pin");
        assert_ne!(pin2, pin, "collision with the agents table redraws");
        assert!(valid_pair(&pin2, &ADJECTIVES, &NOUNS).is_some(), "{pin2}");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// S9 (ADR-0018 D1a): the peer-aware spawn template injects the mail
    /// identity — now the adj+noun PIN, not the slug.
    #[test]
    fn peer_prompt_injects_mail_identity() {
        let tpl = "You are {name}. Your mail identity is {mail_identity}. Budget {poll_interval_minutes}/{poll_budget_minutes}/{max_peer_prompts}";
        let out = peer_prompt_template(tpl, "flywheel-toron-oc4", "DustySparrow", 10, 60, 5);
        assert!(out.contains("Your mail identity is DustySparrow"), "{out}");
        assert!(!out.contains('{'), "no literal placeholders: {out}");
    }
}

/// Block until a thread gains a message newer than the current latest
/// (thread = a message id or a free-form thread id; search by thread_id).
/// Polls `search_messages(thread_id=...)` every `interval_secs`; prints the
/// newest message and exits 0 on the first NEW arrival, 1 on timeout.
fn cmd_mail_watch(
    cfg: &Config,
    project: &str,
    thread_id: &str,
    as_agent: Option<&str>,
    timeout_secs: u64,
    interval_secs: u64,
) -> Result<()> {
    let key = project_dir_str(cfg, project)?;
    let mail = mail_as(cfg, &key, as_agent)?;
    mail.ensure_project(&key)?;
    // Watch via fetch_inbox + CLIENT-side thread match: FTS5 tokenizes on
    // hyphens and letter/digit boundaries, so hyphenated thread ids are not
    // reliably phrase-searchable (e2e-verified). The inbox projection
    // carries `thread_id` per row — exact matching, no FTS dependency.
    let latest = |m: &mail_shim::Mail| -> Result<Option<Value>> {
        let v = m.inbox_filtered(
            &key,
            &m.name,
            &mail_shim::MailInboxFilter {
                unread_only: false,
                include_bodies: true,
                limit: 200,
                ..Default::default()
            },
        )?;
        Ok(v.as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .find(|row| {
                row.get("thread_id")
                    .and_then(|t| t.as_str())
                    .map(|t| t == thread_id)
                    .unwrap_or(false)
            }))
    };
    let start = std::time::Instant::now();
    let seen: Option<String> =
        latest(&mail)?.and_then(|m| m.get("id").and_then(|i| i.as_str()).map(String::from));
    eprintln!(
        "flywheel: watching thread {thread_id} (latest {}); Ctrl-C to abort",
        seen.as_deref().unwrap_or("<none>")
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval_secs));
        if timeout_secs > 0 && start.elapsed().as_secs() >= timeout_secs {
            eprintln!("flywheel: no new message in thread {thread_id} within {timeout_secs}s");
            std::process::exit(1);
        }
        match latest(&mail)? {
            Some(m) => {
                let id = m.get("id").and_then(|i| i.as_str()).map(String::from);
                if id.as_deref() != seen.as_deref() {
                    print_json(&json!({ "thread": thread_id, "new_message": m }));
                    return Ok(());
                }
            }
            None => {
                if seen.is_none() {
                    eprintln!("flywheel: thread {thread_id} has no messages yet");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Real-time collab primitive: send an urgent ask to a peer, then block for
/// the in-thread reply and print it. Combines `mail send` (urgent, ack) +
/// `mail watch` — the A-asks-B pattern as one command. Exit 0 on reply,
/// 1 on timeout.
fn cmd_collab(
    cfg: &Config,
    project: &str,
    to: &str,
    subject: &str,
    body: &str,
    timeout_secs: u64,
    as_agent: Option<&str>,
) -> Result<()> {
    let key = project_dir_str(cfg, project)?;
    let mail = mail_as(cfg, &key, as_agent)?;
    mail.ensure_project(&key)?;
    let thread_id = format!("collab-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S"));
    let targets: Vec<String> = to
        .split(',')
        .map(|s| resolve_mail_target(s.trim()))
        .collect();
    let sent = mail.send(
        &key,
        &targets,
        subject,
        body,
        "urgent",
        true,
        Some(&thread_id),
    )?;
    let msg_id = sent
        .get("id")
        .or_else(|| sent.get("message_id"))
        .and_then(|i| i.as_str())
        .unwrap_or(&thread_id)
        .to_string();
    // bd-a7j: record the reply expectation so a durable loop run verifies the
    // thread actually moved after the agent settles (re-prompt + escalate on
    // silence). Best-effort — a registry write failure must not lose the ask.
    {
        let mut entries = state::load_expected_replies(project);
        let ts = crate::state::now_iso();
        for t in &targets {
            let agent = t.clone();
            entries.retain(|e| e.agent != agent); // one outstanding ask per agent
            entries.push(state::ExpectedReply {
                agent,
                thread: thread_id.clone(),
                head: Some(msg_id.clone()),
                created_ts: ts.clone(),
            });
        }
        if let Err(e) = state::save_expected_replies(project, &entries) {
            eprintln!("flywheel: cannot save expected-reply registry: {e:#}");
        }
    }
    eprintln!(
        "flywheel: asked {} (thread {thread_id}, msg {msg_id}); waiting up to {timeout_secs}s for the reply…",
        targets.join(", ")
    );
    cmd_mail_watch(cfg, project, &thread_id, as_agent, timeout_secs, 5)
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
                false,
                false,
                false,
                5,
                30,
                3,
                30,
                None,
                1,
            )?;
        } else {
            bail!(
                "workspace gone (run: flywheel spawn {project} --recipe <name> or flywheel resume {project} --recipe daily)"
            );
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
                false,
                false,
                false,
                5,
                30,
                3,
                30,
                None,
                1,
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
        false,
        false,
        false,
        5,
        30,
        3,
        30,
        None,
        1,
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

/// Parse `br ready --json` (array) into `ReadyBead`s. Shared by the `__ready`
/// leaf and the sync-loop `--work` source (single source of truth).
fn parse_ready_beads_json(raw: &str) -> Result<Vec<durable::ReadyBead>> {
    let v: Value =
        serde_json::from_str(raw.trim()).map_err(|e| anyhow!("br ready bad JSON: {e}: {raw}"))?;
    let beads: Vec<Value> = v.as_array().cloned().unwrap_or_default();
    Ok(beads
        .into_iter()
        .map(|b| durable::ReadyBead {
            id: b
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: b
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            assignee: b
                .get("assignee")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            // ADR-0019 (bd-w1-verify-fence): the single-line command from the
            // bead's `## VERIFY` fence; None = missing/malformed -> NOT ready.
            verify: b
                .get("description")
                .and_then(Value::as_str)
                .and_then(durable::parse_verify_fence),
        })
        .collect())
}

/// Parse `br list --status in_progress --json` (wrapped under "issues").
/// Shared by the `__inprogress` leaf and the sync-loop `--work` source.
fn parse_inprogress_beads_json(raw: &str) -> Result<Vec<durable::ReadyBead>> {
    let v: Value =
        serde_json::from_str(raw.trim()).map_err(|e| anyhow!("br list bad JSON: {e}: {raw}"))?;
    let list: &Vec<Value> = v
        .get("issues")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .ok_or_else(|| anyhow!("br list output has no issues array: {raw}"))?;
    Ok(list
        .iter()
        .map(|b| durable::ReadyBead {
            id: b
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: b
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            assignee: b
                .get("assignee")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            // ADR-0019 (bd-w1-verify-fence): same fence parse as the ready
            // path — close-detect / claim-watch consumers read it later.
            verify: b
                .get("description")
                .and_then(Value::as_str)
                .and_then(durable::parse_verify_fence),
        })
        .collect())
}

/// Run `br ready --json` in the project dir, returning the ready beads.
/// Caller decides how to treat an error (the sync loop soft-fails).
fn load_ready_beads(cfg: &Config, project: &str) -> Result<Vec<durable::ReadyBead>> {
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
    parse_ready_beads_json(&String::from_utf8_lossy(&out.stdout))
}

/// Run `br list --status in_progress --json` in the project dir.
fn load_inprogress_beads(cfg: &Config, project: &str) -> Result<Vec<durable::ReadyBead>> {
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
    parse_inprogress_beads_json(&String::from_utf8_lossy(&out.stdout))
}

/// Sync-loop `--work` queue refresh (parity with the durable loop's
/// bead-driven mode): resume interrupted in-progress beads first, then hand
/// out fresh ready beads. Each bead is assigned at most once per loop run
/// (the `assigned` set is per-invocation state). Pure — fully testable.
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
        state::unregister_loop(&self.project, self.pid);
    }
}

/// `flywheel loop list`: show the pid registry with live/dead status.
fn cmd_loop_list() -> Result<()> {
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
fn cmd_loop_prune() -> Result<()> {
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

/// `flywheel loop doctor`: health report for loop infrastructure (bd-eign.4).
/// Logic lives in durable::loop_doctor; this is a thin runtime + render stub.
fn cmd_loop_doctor(cfg: &Config, fix: bool, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let report = rt.block_on(durable::loop_doctor(cfg, fix))?;
    if json {
        print_json(&report);
        return Ok(());
    }
    let resonate_ok = report["resonate"]["ok"] == true;
    println!(
        "Resonate: {}",
        if resonate_ok {
            output::ok("connected")
        } else {
            output::error("UNREACHABLE")
        }
    );
    if !resonate_ok {
        println!(
            "  {}",
            report["resonate"]["error"]
                .as_str()
                .unwrap_or("unknown error")
        );
    }
    let stale = report["stale_invocations"]["count"].as_u64().unwrap_or(0);
    println!(
        "Stale pending invocations: {}",
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
            "  {} [{}] {} (pid {} alive={}, promise {})",
            e["project"].as_str().unwrap_or("?"),
            e["verdict"].as_str().unwrap_or("?"),
            e["invocation"].as_str().unwrap_or("?"),
            e["pid"].as_u64().unwrap_or(0),
            e["process_alive"].as_bool().unwrap_or(false),
            e["promise"].as_str().unwrap_or("?")
        );
    }
    if fix {
        let rejected = report["fixed"]["rejected"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        let pruned = report["fixed"]["pruned"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        println!(
            "Fixed: rejected {} stale invocation(s), pruned {} orphaned pid(s)",
            rejected, pruned
        );
    } else {
        println!(
            "{}",
            output::dim("re-run with --fix to reject stale invocations + prune orphaned pids")
        );
    }
    Ok(())
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
    engine: bool,
    force: bool,
) -> Result<()> {
    if interval_secs < 5 {
        bail!("--interval must be >= 5 seconds");
    }
    // single-instance guard (bd-fw-stale-loop-processes-gac): refuse while a
    // live loop process owns this project; --force takes over deliberately.
    // The pid entry is unregistered on exit via LoopPidGuard (Drop), so a
    // crashed loop leaves a stale entry that the next start replaces (dead
    // pid = litter, not a guard).
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
            unknown_detect_grace_polls: cfg.unknown_detect_grace_polls(),
            drain_grace_polls: cfg.drain_grace_polls(),
            claim_nudge_after_polls: cfg.claim_nudge_after_polls(),
            resonate_url: String::new(),
            force,
        };
        let rt = tokio::runtime::Runtime::new()?;
        if engine {
            // bd-e0xk: the loop as a WorkflowEngine durable function — the
            // coexistence path. Dispatches run as `loop-dispatch` engine
            // workflows (unified vocabulary); poll + plan phases unchanged.
            println!(
                "loop: engine-backed swarm monitor for {} (bd-e0xk coexistence path; resume: flywheel loop run {project} --engine)",
                dim(project)
            );
            let result = rt.block_on(durable::run_engine_loop(cfg, params))?;
            println!("loop: {result}");
            log(
                project,
                EventKind::Loop,
                json!({ "status": "complete", "engine": true, "result": result }),
            );
            return Ok(());
        }
        let result = rt.block_on(durable::run_durable_loop(cfg, params))?;
        println!("loop: {result}");
        log(
            project,
            EventKind::Loop,
            json!({ "status": "complete", "result": result }),
        );
        return Ok(());
    }

    // --work (bead-driven): the queue is sourced from `br ready` each poll, so
    // the empty-queue warning only applies when NOT in work mode. Each bead is
    // assigned at most once per loop run (bd-fw-loop-sync-work-zero-way).
    let mut assigned: Vec<String> = Vec::new();
    // ADR-0019 (bd-w1-verify-fence): not-ready beads (missing/malformed
    // `## VERIFY` fence) whose planner-mail already fired this run.
    let mut not_ready_mailed: std::collections::HashSet<String> = std::collections::HashSet::new();
    if queue_items.is_empty() && nudge.is_none() && !work {
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
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            bail!(
                "loop timed out after {}s (queue remaining: {})",
                timeout_ms.unwrap_or(0) / 1000,
                queue_items.len()
            );
        }

        // bead-driven mode (parity with the durable loop): refresh the queue
        // from the tracker each poll — interrupted work first, then fresh
        // ready beads. Soft-fail: a tracker hiccup yields no new work this
        // tick, never a loop crash.
        if work {
            match load_inprogress_beads(cfg, project) {
                Ok(inprog) => {
                    let to_resume: Vec<String> = work_prompts(&inprog, &[], &mut assigned, project);
                    queue_items.extend(to_resume);
                }
                Err(e) => eprintln!("flywheel: br inprogress failed (continuing): {e:#}"),
            }
            match load_ready_beads(cfg, project) {
                Ok(ready) => {
                    // ADR-0019 (bd-w1-verify-fence): only beads with a legal
                    // `## VERIFY` fence are dispatched; not-ready beads are
                    // mailed to the planner once per loop run.
                    let (dispatchable, not_ready) = durable::partition_ready(&ready);
                    let ok: Vec<durable::ReadyBead> = dispatchable.into_iter().cloned().collect();
                    let to_assign: Vec<String> = work_prompts(&[], &ok, &mut assigned, project);
                    queue_items.extend(to_assign);
                    for bead in &not_ready {
                        if not_ready_mailed.insert(bead.id.clone()) {
                            if let Err(e) =
                                cmd_internal_mail_not_ready(cfg, project, &bead.id, &bead.title)
                            {
                                eprintln!(
                                    "flywheel: mail-not-ready for {} failed (continuing): {e:#}",
                                    bead.id
                                );
                            }
                        }
                    }
                }
                Err(e) => eprintln!("flywheel: br ready failed (continuing): {e:#}"),
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
                    // bd-gsvn.1: the sync loop routes through the same
                    // dispatch leaf as the durable loop + pane runner —
                    // __send --profile conditions the prompt with the
                    // agent's profile behavior preamble (Spec C) and logs
                    // the run. A plain swarm agent (no profile in the spawn
                    // registry) sends raw, byte-identical to legacy.
                    let agent_profile = state::load_spawn_registry(&project_dir_str(cfg, project)?)
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
                        eprintln!("flywheel: send to {name} failed: {raw}");
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
                bail!(
                    "loop: agents blocked (on_blocked=continue; pass --on-blocked stop|mail to control)"
                );
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
    fn work_prompts_resumes_inprogress_before_fresh() {
        let inprog = vec![
            durable::ReadyBead {
                id: "bd-r1".into(),
                title: "resume me".into(),
                assignee: None,
                verify: None,
            },
            durable::ReadyBead {
                id: "bd-r2".into(),
                title: "resume me too".into(),
                assignee: None,
                verify: None,
            },
        ];
        let ready = vec![durable::ReadyBead {
            id: "bd-f1".into(),
            title: "fresh".into(),
            assignee: None,
            verify: None,
        }];
        let mut assigned = Vec::new();
        let prompts = work_prompts(&inprog, &ready, &mut assigned, "flywheel");
        // resume first, then fresh
        assert_eq!(prompts.len(), 3);
        assert!(prompts[0].contains("GOAL Resume bd-r1"));
        assert!(prompts[1].contains("GOAL Resume bd-r2"));
        assert!(prompts[2].contains("GOAL Execute bd-f1"));
        assert_eq!(assigned, vec!["bd-r1", "bd-r2", "bd-f1"]);
        // second call assigns nothing new (dedup)
        let again = work_prompts(&inprog, &ready, &mut assigned, "flywheel");
        assert!(again.is_empty());
    }

    #[test]
    fn work_prompts_skips_already_assigned() {
        let inprog = vec![durable::ReadyBead {
            id: "bd-r1".into(),
            title: "x".into(),
            assignee: None,
            verify: None,
        }];
        let ready = vec![durable::ReadyBead {
            id: "bd-r1".into(),
            title: "x".into(),
            assignee: None,
            verify: None,
        }];
        let mut assigned = vec!["bd-r1".to_string()];
        let prompts = work_prompts(&inprog, &ready, &mut assigned, "flywheel");
        assert!(prompts.is_empty());
    }

    #[test]
    fn work_prompts_builds_self_contained_prompts() {
        let ready = vec![durable::ReadyBead {
            id: "bd-42".into(),
            title: "fix auth".into(),
            assignee: None,
            verify: None,
        }];
        let mut assigned = Vec::new();
        let prompts = work_prompts(&[], &ready, &mut assigned, "flywheel");
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        assert!(p.contains("bd-42"));
        assert!(p.contains("GOAL Execute bd-42: fix auth."));
        // the brief carries the standing-orders path, not the lifecycle novel
        // (grep-verifiable per beads workflow)
        assert!(p.contains("flywheel/.flywheel/standing-orders.md"));
        assert!(!p.contains("br update"));
    }

    #[test]
    fn parse_ready_beads_json_handles_array_shape() {
        let raw = r#"[{"id":"bd-a","title":"one"},{"id":"bd-b","title":"two"}]"#;
        let beads = parse_ready_beads_json(raw).unwrap();
        assert_eq!(beads.len(), 2);
        assert_eq!(beads[0].id, "bd-a");
        assert_eq!(beads[1].title, "two");
    }

    #[test]
    fn parse_inprogress_beads_json_handles_issues_wrapper() {
        let raw = r#"{"issues":[{"id":"bd-c","title":"three"}],"total":1}"#;
        let beads = parse_inprogress_beads_json(raw).unwrap();
        assert_eq!(beads.len(), 1);
        assert_eq!(beads[0].id, "bd-c");
        // bare-array fallback (some br versions)
        let beads = parse_inprogress_beads_json(r#"[{"id":"bd-d","title":"four"}]"#).unwrap();
        assert_eq!(beads[0].id, "bd-d");
    }

    /// bd-627z.1: the in-progress parser carries the assignee (drives the
    /// stranded-reclaim + scoped-completion decision); absent/empty → None.
    #[test]
    fn parse_inprogress_beads_json_carries_assignee() {
        let raw = r#"{"issues":[
            {"id":"bd-a","title":"one","assignee":"oc"},
            {"id":"bd-b","title":"two","assignee":"captain"},
            {"id":"bd-c","title":"three"},
            {"id":"bd-d","title":"four","assignee":""}
        ]}"#;
        let beads = parse_inprogress_beads_json(raw).unwrap();
        assert_eq!(beads[0].assignee.as_deref(), Some("oc"));
        assert_eq!(beads[1].assignee.as_deref(), Some("captain"));
        assert_eq!(beads[2].assignee, None, "absent assignee -> None");
        assert_eq!(beads[3].assignee, None, "empty assignee -> None");
    }

    // ---- ADR-0019: VERIFY fence -> ReadyBead.verify -----------------------

    /// `br ready --json` rows carry the description; the fence parses into
    /// `ReadyBead.verify`. Missing/malformed fence -> verify = None (the
    /// bead is NOT ready — refuse-to-dispatch).
    #[test]
    fn verify_fence_parse_ready_beads_json_populates_verify() {
        // built via json! so the `## VERIFY` + fence bytes escape cleanly
        let raw = serde_json::to_string(&serde_json::json!([
            {"id":"bd-a","title":"one","description":"## VERIFY\n```\ncargo test -p flywheel verify_fence\n```\n"},
            {"id":"bd-b","title":"two","description":"no fence here"}
        ]))
        .unwrap();
        let beads = parse_ready_beads_json(&raw).unwrap();
        assert_eq!(beads.len(), 2);
        assert_eq!(
            beads[0].verify.as_deref(),
            Some("cargo test -p flywheel verify_fence"),
            "valid fence -> verify command"
        );
        assert_eq!(beads[1].verify, None, "no fence -> not ready");
        // multi-line fence -> not ready
        let raw_multi = serde_json::to_string(&serde_json::json!([
            {"id":"bd-c","title":"three","description":"## VERIFY\n```\na\nb\n```\n"}
        ]))
        .unwrap();
        let beads = parse_ready_beads_json(&raw_multi).unwrap();
        assert_eq!(beads[0].verify, None, "multi-line fence -> not ready");
    }

    /// ADR-0019 acceptance: a bead without a fence must be refused at
    /// dispatch AND the planner must be mailed on the bead's thread. The
    /// not-ready mail lands in the planner's inbox with thread = bead id.
    #[test]
    fn verify_fence_not_ready_bead_mails_planner_on_bead_thread() {
        use crate::backend::MailSurface as _;
        let mail = crate::mail_shim::Mail::from_config(None)
            .expect("mail ok")
            .expect("mail Some");
        let backend = crate::backend::toron::ToronMailBackend::new(mail);
        let project = "verify-fence-mail";
        backend::run_mail(backend.ensure_project(project)).expect("ensure");
        // the surface sends AS its own identity — it needs a key in-project
        let sender = backend.agent_name().to_string();
        let sender_profile = spawn_agent_profile(&sender, "opencode", project);
        backend::run_mail(backend.register_agent(project, &sender_profile))
            .expect("register sender");
        let profile = spawn_agent_profile("PlannerOne", "opencode", project);
        backend::run_mail(backend.register_agent(project, &profile)).expect("register");
        let receipt = send_not_ready_mail(
            &backend,
            project,
            "bd-fence-missing",
            "some bead",
            "PlannerOne",
        )
        .expect("not-ready mail sends");
        assert!(receipt.count >= 1, "delivered: {receipt:?}");
        let items = backend::run_mail(backend.inbox(&backend::InboxQuery {
            project: project.into(),
            agent: "PlannerOne".into(),
            unread_only: false,
            limit: Some(10),
            include_bodies: false,
            urgent_only: false,
            since_ts: None,
        }))
        .expect("inbox");
        let m = items
            .iter()
            .find(|m| m.subject.contains("NOT ready"))
            .expect("planner received the not-ready mail");
        assert_eq!(
            m.thread_id.as_ref().map(|t| t.as_str()),
            Some("bd-fence-missing"),
            "mail rides the bead's thread (ADR-0019 Spec 0)"
        );
    }

    /// The planner to notify is the bead's creator (`br show --json` →
    /// `created_by`), falling back to the configured identity when the
    /// creator cannot be resolved.
    #[test]
    fn verify_fence_resolve_planner_uses_bead_creator() {
        let dir = std::env::temp_dir().join(format!("verify-fence-planner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let init = std::process::Command::new("br")
            .arg("init")
            .current_dir(&dir)
            .output()
            .expect("br init");
        assert!(init.status.success(), "br init: {init:?}");
        let created = std::process::Command::new("br")
            .args(["create", "--title", "fixture", "--description", "body"])
            .current_dir(&dir)
            .output()
            .expect("br create");
        assert!(created.status.success(), "br create: {created:?}");
        let list = std::process::Command::new("br")
            .args(["list", "--json"])
            .current_dir(&dir)
            .output()
            .expect("br list");
        let v: Value = serde_json::from_slice(&list.stdout).expect("br list json");
        let id = v["issues"][0]["id"].as_str().expect("bead id").to_string();
        let dir_str = dir.to_string_lossy().to_string();
        let planner = resolve_planner_for_bead(&dir_str, &id, "FallbackIdentity");
        assert!(
            !planner.is_empty() && planner != "FallbackIdentity",
            "creator resolved, got: {planner}"
        );
        // unresolvable bead -> fallback
        assert_eq!(
            resolve_planner_for_bead(&dir_str, "bd-does-not-exist", "FallbackIdentity"),
            "FallbackIdentity"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_ready_beads_json_rejects_garbage() {
        assert!(parse_ready_beads_json("not json").is_err());
    }

    #[test]
    fn bare_short_group_matches_detects_crew() {
        let names = vec![
            "flywheel-proj-oc".to_string(),
            "flywheel-proj-oc2".to_string(),
            "flywheel-proj-oc3".to_string(),
        ];
        let group = bare_short_group_matches("proj", "oc", &names).expect("oc broadens");
        assert_eq!(group.len(), 3);
        assert!(group.contains(&"flywheel-proj-oc".to_string()));
        assert!(group.contains(&"flywheel-proj-oc3".to_string()));
        // numbered short is exact — no group
        assert!(bare_short_group_matches("proj", "oc2", &names).is_none());
        // concrete name is exact — no group
        assert!(bare_short_group_matches("proj", "flywheel-proj-oc", &names).is_none());
        // "all" is not a bare short — no group
        assert!(bare_short_group_matches("proj", "all", &names).is_none());
    }

    #[test]
    fn bare_short_group_matches_single_agent_is_fine() {
        let names = vec!["flywheel-proj-oc".to_string()];
        assert!(bare_short_group_matches("proj", "oc", &names).is_none());
    }

    #[test]
    fn kill_key_detects_only_kill_keys() {
        // bd-fw-interrupt-kills-agent-or3: ctrl+c variants are kill keys;
        // Escape and other soft keys are not.
        assert_eq!(kill_key(&["Escape"]), None);
        assert_eq!(kill_key(&["ctrl+c"]), Some("ctrl+c"));
        assert_eq!(kill_key(&["ctrl-c"]), Some("ctrl-c"));
        assert_eq!(kill_key(&["^c"]), Some("^c"));
        assert_eq!(kill_key(&["Escape", "ctrl+c"]), Some("ctrl+c"));
        assert_eq!(kill_key(&[]), None);
    }

    #[test]
    fn slugify_label_normalizes() {
        // bd-vik C: labels become lowercase alphanumerics + hyphens
        let (s, changed) = slugify_label("ADR-0008-debate");
        assert_eq!(s, "adr-0008-debate");
        assert!(changed);
        let (s, _) = slugify_label("My Cool  Label!");
        assert_eq!(s, "my-cool-label");
        let (s, _) = slugify_label("--leading-and-trailing--");
        assert_eq!(s, "leading-and-trailing");
        let (s, changed) = slugify_label("flywheel-toron");
        assert_eq!(s, "flywheel-toron");
        assert!(!changed, "already a slug");
        let (s, _) = slugify_label("!!!");
        assert_eq!(s, "", "no alphanumerics -> empty (caller rejects)");
    }

    #[test]
    fn legacy_loop_project_rewritten_to_run() {
        let args = vec![
            "flywheel".to_string(),
            "loop".to_string(),
            "toron".to_string(),
            "--work".to_string(),
        ];
        let out = rewrite_legacy_loop_args(&args);
        assert_eq!(out, vec!["flywheel", "loop", "run", "toron", "--work"]);
    }

    #[test]
    fn legacy_loop_help_keeps_parent_context() {
        let args = vec![
            "flywheel".to_string(),
            "loop".to_string(),
            "--help".to_string(),
        ];
        assert_eq!(rewrite_legacy_loop_args(&args), args);
        let args = vec!["flywheel".to_string(), "loop".to_string(), "-h".to_string()];
        assert_eq!(rewrite_legacy_loop_args(&args), args);
    }

    #[test]
    fn canonical_loop_subcommands_untouched() {
        for sub in ["run", "status", "list", "prune"] {
            let args = vec!["flywheel".to_string(), "loop".to_string(), sub.to_string()];
            assert_eq!(rewrite_legacy_loop_args(&args), args, "{sub}");
        }
    }

    #[test]
    fn non_loop_commands_untouched() {
        let args = vec![
            "flywheel".to_string(),
            "send".to_string(),
            "toron".to_string(),
        ];
        assert_eq!(rewrite_legacy_loop_args(&args), args);
    }

    #[test]
    fn bare_short_group_matches_comma_list() {
        let names = vec![
            "flywheel-proj-oc".to_string(),
            "flywheel-proj-oc2".to_string(),
            "flywheel-proj-pi".to_string(),
        ];
        // oc,pi → oc matches oc+oc2, pi matches pi → all three matched
        let group = bare_short_group_matches("proj", "oc,pi", &names).unwrap();
        assert_eq!(group.len(), 3);
        assert!(group.contains(&"flywheel-proj-pi".to_string()));
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
    fn done_agent_pops_queue_like_idle() {
        // bd-fw-done-agents-idle-only-iq3: a task-completed agent takes the
        // next bead — dispatch must include `done`, not only `idle`.
        let states = vec![st("a", AgentStatus::Done)];
        let mut q = vec!["p1".into(), "p6".into()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1);
        match &actions[0] {
            LoopAction::Send { name, text } => {
                assert_eq!(name, "a");
                assert_eq!(text, "p1");
            }
            other => panic!("expected Send for done agent, got {other:?}"),
        }
        // and it takes a nudge when the queue is empty
        let states = vec![st("a", AgentStatus::Done)];
        let mut q2: Vec<String> = vec![];
        let actions = plan_actions(&states, &mut q2, Some("continue"));
        assert!(
            matches!(actions[0], LoopAction::Nudge { .. }),
            "{actions:?}"
        );
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
        // bare short broadens: oc matches oc, oc2, oc10
        let want_oc = WantToken {
            name: format!("{base}oc"),
            broaden: true,
            short: None,
            ordinal: 1,
        };
        assert!(name_matches_want(&format!("{base}oc"), &want_oc));
        assert!(name_matches_want(&format!("{base}oc2"), &want_oc));
        assert!(name_matches_want(&format!("{base}oc10"), &want_oc));
        // prefix collision: ocr must NOT match oc
        assert!(!name_matches_want(&format!("{base}ocr"), &want_oc));
        assert!(!name_matches_want(&format!("{base}ocx"), &want_oc));
        // concrete full name is EXACT: flywheel-proj-oc must NOT match oc2
        let want_exact = WantToken {
            name: format!("{base}oc"),
            broaden: false,
            short: None,
            ordinal: 1,
        };
        assert!(name_matches_want(&format!("{base}oc"), &want_exact));
        assert!(!name_matches_want(&format!("{base}oc2"), &want_exact));
        // numbered short is exact too: oc2 must NOT match oc20
        let want_oc2 = WantToken {
            name: format!("{base}oc2"),
            broaden: false,
            short: None,
            ordinal: 1,
        };
        assert!(name_matches_want(&format!("{base}oc2"), &want_oc2));
        assert!(!name_matches_want(&format!("{base}oc20"), &want_oc2));
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
pub(crate) fn resonate_health(cfg: &Config) -> Result<()> {
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

pub(crate) fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── preflight ──────────────────────────────────────────────────────────────

/// Run preflight checks and bail if any fail. No-op when `skip` is true.
fn preflight_gate(
    cfg: &Config,
    skip: bool,
    mode: preflight::Mode,
    project: Option<&str>,
) -> Result<()> {
    if skip {
        return Ok(());
    }
    let checks = preflight::run(cfg, mode, project);
    if preflight::all_pass(&checks) {
        return Ok(());
    }
    eprint!("{}", preflight::render(&checks));
    bail!("preflight failed — fix the above or pass --skip-preflight");
}

/// `flywheel preflight --mode <m> [--project <p>] [--work] [--json]`
fn cmd_preflight(
    cfg: &Config,
    mode: &str,
    project: Option<&str>,
    work: bool,
    json: bool,
) -> Result<()> {
    let pf_mode = match mode {
        "steer" => preflight::Mode::steer(),
        "loop" if work => preflight::Mode::loop_work(),
        "loop" => preflight::Mode::loop_queue(),
        "workflow" => preflight::Mode::workflow(),
        "cron" => preflight::Mode::cron(),
        other => bail!("unknown preflight mode: {other} (steer|loop|workflow|cron)"),
    };
    let checks = preflight::run(cfg, pf_mode, project);
    if json {
        print_json(&preflight::to_json(&checks));
    } else {
        print!("{}", preflight::render(&checks));
    }
    if !preflight::all_pass(&checks) {
        std::process::exit(1);
    }
    Ok(())
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
        "omp" => Some(
            "- Config + sessions: ~/.omp (agent sessions under ~/.omp/agent/sessions/).\n- Prompts live in ~/.omp/agent/prompts/beads-*.md (generated — re-run `flywheel\n  integration omp` after a rebuild to refresh; never hand-edit the generated files).\n- omp CLI works standalone. In herdr, omp exits on PTY stdin EOF (bd-ude8 / eign.7):\n  a context-respawn that tears the PTY down kills the agent. Flywheel layers already\n  inject model/effort args and pass a pane id (not a workspace id). Do not flip omp\n  defaults; remaining layer is herdr PTY lifetime.",
        ),
        _ => None,
    }
}

/// The shared beads-agent BODY — the lifecycle, coordination rules, and
/// context contract that all agent kinds follow. Every phase prompt embeds
/// this body (single source of truth); per-phase focus + per-tool frontmatter
/// live in the phase/frontmatter templates below. Edit the templates in
/// `integrations/` and rebuild — re-run `flywheel integration <kind>` after
/// a rebuild to update the installed prompt files.
const INTEGRATION_BODY: &str = include_str!("../integrations/beads-body.md.tpl");

/// The canonical 5-command set: phase name → description (frontmatter).
/// Each phase prompt = per-tool frontmatter + phase focus + the shared body.
const PROMPT_PHASES: &[(&str, &str)] = &[
    (
        "triage",
        "Find ready bead work — triage the tracker for open, unblocked, dependency-ordered tasks.",
    ),
    (
        "execute",
        "Execute one ready bead end-to-end — claim, reserve, announce, implement, verify, close, sync, handoff.",
    ),
    (
        "review",
        "Review a bead's work for compliance before closing — does it actually satisfy the acceptance criteria?",
    ),
    (
        "commit",
        "Close a completed bead and sync the tracker — close with honest reason, export, commit .beads/, hand off.",
    ),
    (
        "create",
        "Create a new bead from a plan, suggestion, or discovered work — granular, dependency-ordered, self-documenting.",
    ),
];

/// Per-tool prompt frontmatter (tool-specific format) + {{PHASE_DESCRIPTION}}.
fn prompt_frontmatter(kind: &str) -> Option<&'static str> {
    match kind {
        "opencode" => Some(include_str!(
            "../integrations/prompts/frontmatter-opencode.md.tpl"
        )),
        "pi" => Some(include_str!(
            "../integrations/prompts/frontmatter-pi.md.tpl"
        )),
        "grok" => Some(include_str!(
            "../integrations/prompts/frontmatter-grok.md.tpl"
        )),
        "omp" => Some(include_str!(
            "../integrations/prompts/frontmatter-omp.md.tpl"
        )),
        _ => None,
    }
}

/// Per-phase prompt template: `{{TOOL_FRONTMATTER}}` + phase focus +
/// `{{BODY}}` (the shared body).
fn prompt_phase_template(phase: &str) -> Option<&'static str> {
    match phase {
        "triage" => Some(include_str!("../integrations/prompts/beads-triage.md.tpl")),
        "execute" => Some(include_str!("../integrations/prompts/beads-execute.md.tpl")),
        "review" => Some(include_str!("../integrations/prompts/beads-review.md.tpl")),
        "commit" => Some(include_str!("../integrations/prompts/beads-commit.md.tpl")),
        "create" => Some(include_str!("../integrations/prompts/beads-create.md.tpl")),
        _ => None,
    }
}

/// The workflow slash-command set (bd-oj9o #3, bd-ucd4): command name →
/// (target workflow, frontmatter description). The POPULAR user-facing
/// workflows — debate, plan, plan-review, brainstorm, onboard, triage-deep.
/// Behind-the-scenes workflows (cron, beads, goal) deliberately get NO
/// command templates.
const PROMPT_WORKFLOWS: &[(&str, &str, &str)] = &[
    (
        "debate",
        "debate",
        "Run the debate workflow — adversarial synthesis: reviewers brief, an oracle decides.",
    ),
    (
        "plan",
        "plan",
        "Run the plan workflow — goal → bead-ready plan via the planner.",
    ),
    (
        "plan-review",
        "plan-review",
        "Run the plan-review workflow — review a plan bead-by-bead before implementation.",
    ),
    (
        "brainstorm",
        "brainstorm",
        "Run the brainstorm workflow — divergent ideation: oracles fan out, a planner reduces.",
    ),
    (
        "onboard",
        "onboard",
        "Run the onboard workflow — cold-start a newcomer: repo map, seed context, first task.",
    ),
    (
        "triage",
        "triage-deep",
        "Run the triage-deep workflow — bv robot-triage, prioritize + assign, spawn the crew.",
    ),
    (
        "captain-audit",
        "captain-audit",
        "Run the captain-audit workflow — read-only analysis: scout gathers evidence, oracle synthesizes, reviewer challenges; the report is written to report_path.",
    ),
];

/// Per-workflow command template: `{{TOOL_FRONTMATTER}}` + the workflow
/// focus. Standalone — no beads body (these are workflow commands, not bead
/// lifecycle commands).
fn prompt_workflow_template(name: &str) -> Option<&'static str> {
    match name {
        "debate" => Some(include_str!(
            "../integrations/prompts/workflow-debate.md.tpl"
        )),
        "plan" => Some(include_str!("../integrations/prompts/workflow-plan.md.tpl")),
        "plan-review" => Some(include_str!(
            "../integrations/prompts/workflow-plan-review.md.tpl"
        )),
        "brainstorm" => Some(include_str!(
            "../integrations/prompts/workflow-brainstorm.md.tpl"
        )),
        "onboard" => Some(include_str!(
            "../integrations/prompts/workflow-onboard.md.tpl"
        )),
        "triage" => Some(include_str!(
            "../integrations/prompts/workflow-triage.md.tpl"
        )),
        "captain-audit" => Some(include_str!(
            "../integrations/prompts/workflow-captain-audit.md.tpl"
        )),
        _ => None,
    }
}

/// The prompts directory for one agent kind (the dir the 5 command files
/// are written into). Purely conventional per tool.
pub fn prompts_dir(kind: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = std::path::Path::new(&home);
    Some(match kind {
        "opencode" => home.join(".config/opencode/command"),
        "pi" => home.join(".pi/agent/prompts"),
        "grok" => home.join(".grok/prompts"),
        "omp" => home.join(".omp/agent/prompts"),
        _ => return None,
    })
}

/// Render one phase prompt for one agent kind. Pure: `version` and
/// `generated` are injected by the caller (deterministic tests).
fn render_prompt(kind: &str, phase: &str, version: &str, generated: &str) -> Result<String> {
    let frontmatter = prompt_frontmatter(kind)
        .ok_or_else(|| anyhow!("unknown agent kind: {kind} (known: opencode, pi, grok, omp)"))?;
    let phase_tpl = prompt_phase_template(phase).ok_or_else(|| {
        anyhow!("unknown phase: {phase} (known: triage, execute, review, commit, create)")
    })?;
    let (_, description) = PROMPT_PHASES
        .iter()
        .find(|(p, _)| *p == phase)
        .expect("phase in table");
    let notes = kind_notes(kind).unwrap_or_default();
    let body = INTEGRATION_BODY
        .replace("{{KIND_NOTES}}", notes)
        .replace("{{VERSION}}", version)
        .replace("{{GENERATED}}", generated);
    Ok(phase_tpl
        .replace(
            "{{TOOL_FRONTMATTER}}",
            &frontmatter.replace("{{PHASE_DESCRIPTION}}", description),
        )
        .replace("{{BODY}}", &body)
        .replace("{{VERSION}}", version)
        .replace("{{GENERATED}}", generated))
}

/// Render one workflow command for one agent kind. Pure (deterministic
/// tests): frontmatter (with the workflow's description) + the workflow
/// focus template + version/generated stamps.
fn render_workflow_prompt(
    kind: &str,
    name: &str,
    version: &str,
    generated: &str,
) -> Result<String> {
    let frontmatter = prompt_frontmatter(kind)
        .ok_or_else(|| anyhow!("unknown agent kind: {kind} (known: opencode, pi, grok, omp)"))?;
    let tpl = prompt_workflow_template(name)
        .ok_or_else(|| anyhow!("unknown workflow command: {name} (flywheel integration list)"))?;
    let (_, _, description) = PROMPT_WORKFLOWS
        .iter()
        .find(|(n, _, _)| *n == name)
        .expect("workflow command in table");
    Ok(tpl
        .replace(
            "{{TOOL_FRONTMATTER}}",
            &frontmatter.replace("{{PHASE_DESCRIPTION}}", description),
        )
        .replace("{{VERSION}}", version)
        .replace("{{GENERATED}}", generated))
}

/// `flywheel integration [KIND] [--out PATH]` — generate the 5 beads prompt
/// files (beads-{triage,execute,review,commit,create}.md) for one agent kind
/// (opencode/pi/grok/omp), each from the shared beads-body. With no KIND,
/// list the available kinds. With `--out <dir>`, write the 5 files into that
/// directory instead of the tool's conventional prompts dir.
///
/// The beads SUBAGENT (agents/beads.md) is retired (bd-ra0g.1): the loop
/// drives panes via its self-contained dispatch prompt, and nothing
/// load-bearing loads the subagent — prompts are the delivery surface now.
fn cmd_integration(kind: Option<&str>, out: Option<&Path>) -> Result<()> {
    let Some(kind) = kind else {
        println!("flywheel beads prompts (flywheel {VERSION}):");
        println!("  5 beads commands per kind: beads-{{triage,execute,review,commit,create}}.md");
        println!(
            "  + {} workflow slash commands per kind: workflow-{{{}}}.md",
            PROMPT_WORKFLOWS.len(),
            PROMPT_WORKFLOWS
                .iter()
                .map(|(n, _, _)| *n)
                .collect::<Vec<_>>()
                .join(",")
        );
        for (k, _) in [("opencode", ""), ("pi", ""), ("grok", ""), ("omp", "")] {
            let dir = prompts_dir(k)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<HOME>/.config/...".to_string());
            println!("  {k:9} flywheel integration {k}   -> {dir}/beads-*.md");
        }
        println!(
            "templates: integrations/beads-body.md.tpl + integrations/prompts/*.tpl — edit, rebuild, re-run to update"
        );
        return Ok(());
    };
    // accept the spawn shorts (oc/pi/gk) as well as the full kind names
    let kind = match kind {
        "oc" => "opencode",
        "gk" => "grok",
        other => other,
    };
    let dir = match out {
        Some(p) => p.to_path_buf(),
        None => prompts_dir(kind).ok_or_else(|| {
            anyhow!("unknown agent kind: {kind} (known: opencode, pi, grok, omp)")
        })?,
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create prompts dir {}", dir.display()))?;
    let generated = state::now_iso();
    let mut written = 0usize;
    for (phase, _) in PROMPT_PHASES {
        let rendered = render_prompt(kind, phase, VERSION, &generated)?;
        let path = dir.join(format!("beads-{phase}.md"));
        std::fs::write(&path, &rendered).with_context(|| format!("write {}", path.display()))?;
        println!(
            "wrote {} ({} bytes)",
            dim(&path.display().to_string()),
            rendered.len()
        );
        written += 1;
    }
    // The workflow slash commands (bd-oj9o #3): /workflow-{debate,plan-review,
    // brainstorm} — the popular user-facing workflows; cron excluded.
    for (name, _, _) in PROMPT_WORKFLOWS {
        let rendered = render_workflow_prompt(kind, name, VERSION, &generated)?;
        let path = dir.join(format!("workflow-{name}.md"));
        std::fs::write(&path, &rendered).with_context(|| format!("write {}", path.display()))?;
        println!(
            "wrote {} ({} bytes)",
            dim(&path.display().to_string()),
            rendered.len()
        );
        written += 1;
    }
    println!(
        "flywheel integration {kind}: {written} prompt file(s) in {}",
        dim(&dir.display().to_string())
    );
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
                eprintln!(
                    "flywheel: unknown agent kind '{other}' — use opencode,pi,grok (shorts oc,pi,gk)"
                );
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
    if let Ok(h) = std::env::var("HOME")
        && let Ok(strip) = p.strip_prefix(&h)
    {
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

/// bd-evqs: reason → bead-id parsing for the closed-bead renewal sweep.
#[cfg(test)]
mod renew_sweep_tests {
    use super::*;

    #[test]
    fn bead_id_from_reason_parses_bd_ids() {
        assert_eq!(bead_id_from_reason("bd-oj9o"), Some("bd-oj9o"));
        assert_eq!(
            bead_id_from_reason("bd-oj9o "),
            Some("bd-oj9o"),
            "trailing space trimmed"
        );
        assert_eq!(
            bead_id_from_reason("  bd-12  "),
            Some("bd-12"),
            "leading space trimmed"
        );
        assert_eq!(
            bead_id_from_reason("bd-12: fix auth"),
            Some("bd-12"),
            "first token wins"
        );
    }

    #[test]
    fn bead_id_from_reason_rejects_non_beads() {
        assert_eq!(bead_id_from_reason(""), None);
        assert_eq!(
            bead_id_from_reason("src/foo.rs"),
            None,
            "paths are not beads"
        );
        assert_eq!(bead_id_from_reason("orchestrator-lock"), None);
        assert_eq!(bead_id_from_reason("bd-"), None, "bare prefix is not an id");
        assert_eq!(
            bead_id_from_reason("my-bd-12"),
            None,
            "prefix must start the token"
        );
    }

    /// The sweep predicate: a reservation is stale when its reason parses to
    /// a bead id that is in the CLOSED set. (The filter closure lives in
    /// cmd_internal_renew_leases; this pins the parse + set membership that
    /// drives it.)
    #[test]
    fn closed_set_membership_drives_stale_filter() {
        let closed: std::collections::HashSet<String> =
            ["bd-oj9o".to_string(), "bd-12".to_string()].into();
        let stale: Vec<&str> = ["bd-oj9o", "bd-99", "src/foo.rs", ""]
            .iter()
            .filter(|r| {
                bead_id_from_reason(r)
                    .map(|id| closed.contains(id))
                    .unwrap_or(false)
            })
            .copied()
            .collect();
        assert_eq!(stale, vec!["bd-oj9o"], "only closed-bead reasons are stale");
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

    // H3 (bd-harness-integration-preflight): per-kind integration state
    // (kind -> installed/current/version). Informational — a failed status
    // read warns but does not fail the deps check (the socket surface works
    // regardless of the CLI status read).
    match herdr_shim::integration_status_all() {
        Ok(list) => {
            println!("herdr integrations:");
            for (kind, st) in list {
                println!("  {}", integration_status_line(&kind, &st));
            }
        }
        Err(e) => {
            eprintln!(
                "flywheel: WARNING cannot read `herdr integration status` — integration surface degraded: {e:#}"
            );
            println!("herdr integrations: {}", crate::output::warn("unknown"));
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
            Ok(_) => println!("mail (toron): {}", status_paint(AgentStatus::Idle)),
            Err(e) => {
                ok = false;
                println!("mail (toron): {} ({e:#})", error("FAIL"));
            }
        },
        Ok(None) => println!("mail (toron): {}", dim("not configured (set [mail.toron])")),
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
    use profile::{Profile, ProfileCatalog};

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
                    "effort": p.effort,
                    "smol": p.smol,
                    "slow": p.slow,
                    "plan": p.plan,
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
                "effort": p.effort,
                "smol": p.smol,
                "slow": p.slow,
                "plan": p.plan,
                "role": p.role,
                "backend": cat.backend(name),
                "default_workflow": p.default_workflow,
                "behavior": p.behavior,
                "task_hint": p.task_hint,
                "skills": p.skills,
            }));
        }
        ProfileCmd::Create {
            name,
            harness,
            model,
            role,
            behavior,
            task_hint,
            backend,
            default_workflow,
            effort,
            smol,
            slow,
            plan,
        }
        | ProfileCmd::Edit {
            name,
            harness,
            model,
            role,
            behavior,
            task_hint,
            backend,
            default_workflow,
            effort,
            smol,
            slow,
            plan,
        } => {
            let path = config::config_dir().join("profiles.toml");
            let cat = ProfileCatalog::load()?;
            let base = cat.get(name).cloned().unwrap_or_default();
            let merged = Profile {
                harness: harness.clone().unwrap_or(base.harness),
                model: model.clone().unwrap_or(base.model),
                effort: effort.clone().unwrap_or(base.effort),
                smol: smol.clone().unwrap_or(base.smol),
                slow: slow.clone().unwrap_or(base.slow),
                plan: plan.clone().unwrap_or(base.plan),
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
                if (n == *name || !is_builtin_profile(&n))
                    && let Some(p) = cat.get(&n)
                {
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
        ProfileCmd::Run {
            name,
            workflow,
            inputs,
        } => {
            let cat = ProfileCatalog::load()?;
            let _inputs: Value = match inputs {
                Some(s) if !s.is_empty() => {
                    serde_json::from_str(s).map_err(|e| anyhow!("--inputs must be JSON: {e}"))?
                }
                _ => json!({}),
            };
            // Host-scope resolve: operator identity, no project lookup. The
            // operator npub is best-effort (empty string when unset).
            let operator_npub =
                std::env::var("TORON_OPERATOR_NPUB").unwrap_or_else(|_| "operator".to_string());
            let resolved = cat.resolve(name, None, &operator_npub, &|_, _| None)?;
            let spec = &resolved.spec;

            // Spawn via the configured multiplexer backend (default herdr).
            let backend = backend::multiplexer_backend(cfg)?;
            let agent_name = format!("profile-{name}");
            // bd-l2ky: launch with the profile's model/effort args — the
            // same profile_launch_args as `spawn --profile` (bd-eign.9),
            // never an empty arg list.
            let profile = cat.get(name).expect("resolve validated the profile above");
            let args = profile_launch_args(&spec.harness, profile);
            // Find a pane in the default project workspace, or create one.
            let workspace = resolved_spec_workspace(cfg, backend.as_ref(), &spec.harness)?;
            let info = backend.agent_start(&agent_name, &spec.harness, &workspace, &args)?;
            println!(
                "profile {name} spawned: agent={} pane={} backend={}",
                info.name.as_deref().unwrap_or(&agent_name),
                info.pane_id,
                spec.backend
            );
            // bd-l2ky: record the profiled entry under the orchestrator's
            // host project (host-scope spawn, debate convention) so the
            // loop's `__send --profile` / conditioning resolves this agent.
            let host_project = "flywheel";
            let mail_identity = register_spawned_agent(
                cfg,
                host_project,
                &agent_name,
                &spec.harness,
                &info.pane_id,
            );
            record_spawn_entry(
                cfg,
                host_project,
                state::SpawnEntry::profiled(
                    &info.pane_id,
                    &agent_name,
                    name,
                    &spec.harness,
                    mail_identity,
                ),
            );
            if let Some(wf) = workflow {
                // bd-6zsl: invoke the workflow against the spawned agent —
                // pane mode, --reuse the agent's workspace (same panes, the
                // crew lands beside the profile agent; never reaped).
                let invoke = profile_workflow_invoke(
                    wf.as_str(),
                    inputs.as_deref(),
                    workspace.as_str(),
                    host_project,
                );
                cmd_workflow(cfg, &invoke)?;
            }
        }
        ProfileCmd::Remove { name } => {
            if is_builtin_profile(name) {
                bail!(
                    "cannot remove built-in profile {name:?} — override it via `flywheel profile create {name} ...` instead"
                );
            }
            let path = config::config_dir().join("profiles.toml");
            let cat = ProfileCatalog::load()?;
            if cat.get(name).is_none() {
                bail!("no profile named {name:?}");
            }
            // Rebuild the on-disk local entries WITHOUT the named one (mirrors
            // create/edit: only non-builtin entries are persisted).
            let mut on_disk: BTreeMap<String, profile::Profile> = BTreeMap::new();
            for n in cat.names() {
                if n != *name
                    && !is_builtin_profile(&n)
                    && let Some(p) = cat.get(&n)
                {
                    on_disk.insert(n, p.clone());
                }
            }
            if on_disk.is_empty() {
                // Removing the last local entry → drop the file (clean state).
                let _ = std::fs::remove_file(&path);
            } else {
                let raw = toml::to_string_pretty(&ProfilesFile { profiles: on_disk })?;
                std::fs::write(&path, raw)?;
            }
            println!("removed profile {name}");
        }
    }
    Ok(())
}

/// `flywheel coalition ...` (ADR-0011 Spec 1, P11.3a): the steer surface +
/// manual collab baseline. All collab ops are membership-gated; P11.2's
/// soft-conflict freeze + request-release ladder are the manual resolution
/// path (the auto-proposer is P11.3b).
fn cmd_coalition(cmd: &CoalitionCmd) -> Result<()> {
    use coalition::CoalitionStore;
    use toron_core::mailbox::{ReleaseAsk, ReleaseDecision};

    /// Resolve a crew spec to member profile names (same rule as
    /// `workflow run --crew`): recipes.toml squad, else a single profile.
    /// Comma-separated crews (bd-hkw5.3): multiple squads/profiles — the
    /// cross-project coalition's crews from different repos.
    fn squad_members(crews: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for crew in crews.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            let cat = profile::ProfileCatalog::load()?;
            let recipes = config::Recipes::load()?.recipes;
            let counts = match recipes.get(crew) {
                Some(r) => profile::Squad::from_recipe(r).counts,
                None if cat.get(crew).is_some() => {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert(crew.to_string(), 1u32);
                    m
                }
                None => bail!("unknown squad {crew:?} (flywheel squad list)"),
            };
            for m in counts.keys() {
                if !out.contains(m) {
                    out.push(m.clone());
                }
            }
        }
        Ok(out)
    }

    fn coalitions_root() -> std::path::PathBuf {
        config::state_dir().join("coalitions")
    }

    fn parse_ask(s: &str) -> Result<ReleaseAsk> {
        if s == "release" {
            Ok(ReleaseAsk::Release)
        } else if let Some(secs) = s.strip_prefix("defer:") {
            Ok(ReleaseAsk::Defer {
                defer_secs: secs
                    .parse()
                    .map_err(|e| anyhow!("--ask defer:<secs> parse: {e}"))?,
            })
        } else {
            bail!("--ask must be `release` or `defer:<secs>`, got {s:?}")
        }
    }

    fn parse_decision(s: &str) -> Result<ReleaseDecision> {
        if s == "agreed" {
            Ok(ReleaseDecision::Agreed)
        } else if s == "denied" {
            Ok(ReleaseDecision::Denied)
        } else if let Some(secs) = s.strip_prefix("deferred:") {
            Ok(ReleaseDecision::Deferred {
                defer_secs: secs
                    .parse()
                    .map_err(|e| anyhow!("--decision deferred:<secs> parse: {e}"))?,
            })
        } else {
            bail!("--decision must be agreed|denied|deferred:<secs>, got {s:?}")
        }
    }

    match cmd {
        CoalitionCmd::Register {
            name,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            store.register_agent(name)?;
            if *json {
                print_json(&json!({ "registered": name, "project": project }));
            } else {
                println!("registered agent {name} in project {project}");
            }
            Ok(())
        }
        CoalitionCmd::Form {
            goal,
            crews,
            as_agent,
            member_project,
            project,
            beads,
            tracker,
            json,
        } => {
            let members = squad_members(crews)?;
            let member_projects: BTreeMap<String, String> = member_project
                .iter()
                .map(|kv| {
                    let (member, proj) = kv.split_once('=').ok_or_else(|| {
                        anyhow!("--member-project expects <member>=<project>, got {kv:?}")
                    })?;
                    if member.is_empty() || proj.is_empty() {
                        return Err(anyhow!(
                            "--member-project expects <member>=<project>, got {kv:?}"
                        ));
                    }
                    if !members.contains(&member.to_string()) {
                        return Err(anyhow!(
                            "--member-project member {member} is not in crew {crews:?}"
                        ));
                    }
                    Ok((member.to_string(), proj.to_string()))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let beads: Vec<String> = beads
                .as_deref()
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let store = CoalitionStore::open(project, coalitions_root())?;
            let m = store.form(
                goal,
                as_agent,
                &members,
                &member_projects,
                &beads,
                tracker.as_deref(),
            )?;
            if *json {
                print_json(&serde_json::to_value(&m)?);
            } else {
                println!(
                    "coalition {} formed: goal {}, {} member(s), channel {} + run {}",
                    m.id,
                    m.goal,
                    m.members.len(),
                    m.channel_id,
                    m.run_id
                );
                if !m.member_projects.is_empty() {
                    for (member, proj) in &m.member_projects {
                        println!("  {member} -> project {proj}");
                    }
                }
            }
            Ok(())
        }
        CoalitionCmd::Status {
            id,
            project,
            json,
            digest,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let st = store.status(id)?;
            if *json {
                print_json(&serde_json::to_value(&st)?);
            } else if *digest {
                // bd-hkw5.1 digest mode: one line per member + the board
                // heat (intents / frozen / negotiations) — the captain's
                // quick scan.
                println!(
                    "coalition {} [{}] goal {} captain {}",
                    st.id, st.status, st.goal, st.captain
                );
                let mut by_member: std::collections::BTreeMap<String, (usize, usize, usize)> =
                    std::collections::BTreeMap::new();
                for i in &st.intents {
                    by_member.entry(i.agent_name.clone()).or_default().0 += 1;
                }
                for r in &st.reservations {
                    by_member.entry(r.agent_name.clone()).or_default().1 += 1;
                }
                for f in &st.frozen {
                    by_member.entry(f.agent.clone()).or_default().2 += 1;
                }
                for m in &st.members {
                    let (intents, held, frozen) = by_member.get(m).copied().unwrap_or((0, 0, 0));
                    println!(
                        "  {} [{}] intents {} held {} frozen {}",
                        m,
                        st.roles.get(m).map(String::as_str).unwrap_or("member"),
                        intents,
                        held,
                        frozen
                    );
                }
                println!(
                    "  negotiations: {} | channel events: {} | run: {}",
                    st.negotiations.len(),
                    st.channel.events.len(),
                    st.run
                        .as_ref()
                        .map(|r| r.status.as_str())
                        .unwrap_or("unknown")
                );
                for n in &st.negotiations {
                    println!(
                        "    {} -> {}: {} ({})",
                        n.requester, n.holder, n.path, n.status
                    );
                }
            } else {
                println!(
                    "coalition {} [{}] goal {} captain {}",
                    st.id, st.status, st.goal, st.captain
                );
                println!(
                    "  run: {} ({})",
                    st.run_id,
                    st.run
                        .as_ref()
                        .map(|r| r.status.as_str())
                        .unwrap_or("unknown")
                );
                for (member, proj) in &st.member_projects {
                    println!("  {member} -> project {proj}");
                }
                println!(
                    "  channel: {} ({} event(s) on bus)",
                    st.channel.channel_id,
                    st.channel.events.len()
                );
                println!(
                    "  intents: {} | reservations: {} | frozen: {} | negotiations: {}",
                    st.intents.len(),
                    st.reservations.len(),
                    st.frozen.len(),
                    st.negotiations.len()
                );
                for i in &st.intents {
                    // TTL (bd-hkw5.1): intent expiry as epoch µs (None = no
                    // expiry), plus the declared TTL.
                    let ttl = i
                        .expires_ts_us
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "none".into());
                    println!(
                        "    intent {}: {} @ {} (ttl {}s, expires {})",
                        i.agent_name, i.path_pattern, i.reason, i.ttl_secs, ttl
                    );
                }
                for r in &st.reservations {
                    println!(
                        "    held {}: {} (expires {})",
                        r.agent_name,
                        r.path_pattern,
                        r.expires_ts_us
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "none".into())
                    );
                }
                for f in &st.frozen {
                    println!(
                        "    FROZEN {}: {} (overlaps {})",
                        f.agent,
                        f.path,
                        f.holders.join(", ")
                    );
                }
                for n in &st.negotiations {
                    println!(
                        "    negotiate {} -> {}: {} ({})",
                        n.requester, n.holder, n.path, n.status
                    );
                }
                for b in &st.beads {
                    println!("    bead {}: {}", b.id, b.status);
                }
            }
            Ok(())
        }
        CoalitionCmd::Intent {
            id,
            paths,
            as_agent,
            reason,
            ttl,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.declare_intent(id, as_agent, paths, reason.as_deref(), *ttl)?;
            if *json {
                print_json(&serde_json::to_value(&res)?);
            } else {
                println!(
                    "intent declared: {} path(s) by {as_agent}",
                    res.declared.len()
                );
            }
            Ok(())
        }
        CoalitionCmd::Reserve {
            id,
            paths,
            as_agent,
            ttl,
            exclusive,
            reason,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.reserve(id, as_agent, paths, *ttl, *exclusive, reason.as_deref())?;
            if *json {
                print_json(&serde_json::to_value(&res)?);
            } else {
                println!(
                    "reserve: {} granted, {} hard conflict(s), {} SOFT conflict(s) (freeze)",
                    res.granted.len(),
                    res.conflicts.len(),
                    res.soft_conflicts.len()
                );
            }
            Ok(())
        }
        CoalitionCmd::NegotiateRequest {
            id,
            as_agent,
            target,
            reservation_id,
            ask,
            note,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.negotiate_request(
                id,
                as_agent,
                target,
                reservation_id,
                parse_ask(ask)?,
                note.as_deref(),
            )?;
            if *json {
                print_json(&serde_json::to_value(&res)?);
            } else {
                println!(
                    "request-release sent to {} for {} (message {})",
                    res.holder, res.path_pattern, res.message_id
                );
            }
            Ok(())
        }
        CoalitionCmd::NegotiateRespond {
            id,
            as_agent,
            request_message,
            decision,
            note,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let res = store.negotiate_respond(
                id,
                as_agent,
                request_message,
                parse_decision(decision)?,
                note.as_deref(),
            )?;
            if *json {
                print_json(&json!({
                    "deliveries": res.deliveries.iter().map(|d| json!({
                        "recipient": d.payload.recipient,
                        "event_id": d.payload.event_id,
                    })).collect::<Vec<_>>(),
                    "count": res.count,
                }));
            } else {
                println!(
                    "response delivered to {} recipient(s)",
                    res.deliveries.len()
                );
            }
            Ok(())
        }
        CoalitionCmd::NegotiateStatus {
            id,
            as_agent,
            request_message,
            window,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let st = store.negotiate_status(id, as_agent, request_message, *window)?;
            if *json {
                print_json(&serde_json::to_value(&st)?);
            } else {
                println!("{st:?}");
            }
            Ok(())
        }
        CoalitionCmd::NegotiateEscalate {
            id,
            as_agent,
            request_message,
            note,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            store.negotiate_escalate(id, as_agent, request_message, note.as_deref())?;
            if *json {
                print_json(&json!({ "escalated": true }));
            } else {
                println!("request {request_message} escalated to the captain");
            }
            Ok(())
        }
        CoalitionCmd::List { project, json } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let list = store.list()?;
            if *json {
                print_json(&serde_json::to_value(&list)?);
            } else {
                for m in &list {
                    println!(
                        "{} [{}] goal {} members {}",
                        m.id,
                        m.status,
                        m.goal,
                        m.members.len()
                    );
                }
            }
            Ok(())
        }
        CoalitionCmd::Detect { project, json } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let report = store.detect()?;
            if *json {
                print_json(&serde_json::to_value(&report)?);
            } else {
                if report.overlaps.is_empty() {
                    println!("no intent overlaps on the board");
                }
                for o in &report.overlaps {
                    let tag = if o.non_trivial { "source" } else { "trivial" };
                    println!("overlap: {} <-> {} ({tag})", o.a, o.b);
                    println!("  {} : {}", o.a, o.path_a);
                    println!("  {} : {}", o.b, o.path_b);
                }
                for p in &report.proposals {
                    println!(
                        "proposal [{tier}/{kind}]: {} <-> {} — {}",
                        p.agents[0],
                        p.agents[1],
                        p.rationale,
                        tier = p.tier,
                        kind = serde_json::to_string(&p.kind)
                            .unwrap_or_default()
                            .trim_matches('"')
                    );
                }
                for c in &report.auto_coalesce {
                    println!(
                        "auto-coalesce candidate: {} <-> {} (non-trivial source)",
                        c.a, c.b
                    );
                }
            }
            Ok(())
        }
        CoalitionCmd::AutoCoalesce {
            goal,
            as_agent,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let report = store.detect()?;
            if report.auto_coalesce.is_empty() {
                if *json {
                    print_json(&json!({ "formed": [] }));
                } else {
                    println!("no auto-coalesce candidates (no non-trivial unaffiliated overlap)");
                }
                return Ok(());
            }
            let formed = store.auto_coalesce(goal, as_agent, &report)?;
            if *json {
                print_json(&serde_json::to_value(&formed)?);
            } else {
                for m in &formed {
                    println!(
                        "formed coalition {} (goal {goal}, members {})",
                        m.id,
                        m.members.join(", ")
                    );
                }
            }
            Ok(())
        }
        CoalitionCmd::Disband {
            id,
            beads_closed,
            reason,
            tracker,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            store.disband(id, *beads_closed, reason.as_deref(), tracker.as_deref())?;
            if *json {
                print_json(&json!({ "disbanded": id }));
            } else {
                println!("coalition {id} disbanded");
            }
            Ok(())
        }
        CoalitionCmd::Leaf {
            id,
            as_agent,
            profile,
            scope,
            harness,
            project,
            json,
        } => {
            let store = CoalitionStore::open(project, coalitions_root())?;
            let leaf = store.dispatch_leaf(id, as_agent, profile, scope, harness.as_deref())?;
            if *json {
                print_json(&serde_json::to_value(&leaf)?);
            } else {
                println!(
                    "leaf @{} run {} [{}] {} tok={}+{} {}ms (result posted to coalition {})",
                    leaf.profile,
                    leaf.run_id,
                    leaf.status,
                    leaf.model,
                    leaf.tokens_in,
                    leaf.tokens_out,
                    leaf.duration_ms,
                    id
                );
                if let Some(e) = &leaf.error {
                    eprintln!("leaf error: {e}");
                }
            }
            Ok(())
        }
    }
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
                println!(
                    "no squads defined (edit {})",
                    config::config_dir().join("recipes.toml").display()
                );
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
        SquadCmd::Spawn {
            name,
            project,
            json,
        } => {
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
            let operator_npub =
                std::env::var("TORON_OPERATOR_NPUB").unwrap_or_else(|_| "operator".to_string());
            let backend = backend::multiplexer_backend(cfg)?;
            let mut spawned: Vec<Value> = Vec::new();
            // Registry key for the spawn entries: the squad's project, else
            // the orchestrator's host project (host-scope, debate convention).
            let project_key = project.as_deref().unwrap_or("flywheel");
            for (profile_name, n) in squad.members() {
                let resolved =
                    cat.resolve(profile_name, project.as_deref(), &operator_npub, &|_, _| {
                        None
                    })?;
                let spec = &resolved.spec;
                // bd-l2ky: launch with the member profile's model/effort args
                // — the same profile_launch_args as `spawn --profile`
                // (bd-eign.9), never an empty arg list.
                let profile = cat
                    .get(profile_name)
                    .expect("resolve validated the profile above");
                for i in 0..n {
                    let agent_name = format!("squad-{name}-{profile_name}-{}", i + 1);
                    let workspace = resolved_spec_workspace(cfg, backend.as_ref(), &spec.harness)?;
                    let args = profile_launch_args(&spec.harness, profile);
                    let info =
                        backend.agent_start(&agent_name, &spec.harness, &workspace, &args)?;
                    // bd-l2ky: record the profiled entry so __send --profile
                    // and loop conditioning resolve this member.
                    let mail_identity = register_spawned_agent(
                        cfg,
                        project_key,
                        &agent_name,
                        &spec.harness,
                        &info.pane_id,
                    );
                    record_spawn_entry(
                        cfg,
                        project_key,
                        state::SpawnEntry::profiled(
                            &info.pane_id,
                            &agent_name,
                            profile_name,
                            &spec.harness,
                            mail_identity,
                        ),
                    );
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
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "spawned": spawned }))?
                );
            }
            Ok(())
        }
        SquadCmd::Remove { name } => {
            let mut recipes = config::Recipes::load()?;
            if !recipes.recipes.contains_key(name) {
                bail!("no squad/recipe named {name:?}");
            }
            recipes.recipes.remove(name);
            let path = config::config_dir().join("recipes.toml");
            if recipes.recipes.is_empty() {
                let _ = std::fs::remove_file(&path);
            } else {
                let raw = toml::to_string_pretty(&recipes)?;
                std::fs::write(&path, raw)?;
            }
            println!("removed squad/recipe {name}");
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
        ["cron", "workflow", "pipeline", "loop", "agent"]
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
            let mut runs = all_run_entries()?;
            // Headless run logs (ADR-0010 Spec E, P10.1/P10.4): the leaf
            // writes runs/<run_id>.jsonl — list them alongside the state-log
            // runs so cron outcomes surface in the same dashboard.
            for hl in run_log::read_runs() {
                runs.push((
                    hl.run_id.clone(),
                    state::LogEntry {
                        ts: hl.ts.clone(),
                        kind: state::EventKind::Workflow,
                        project: String::new(),
                        data: json!({
                            "status": hl.status,
                            "tokens": hl.tokens_in + hl.tokens_out,
                            "model": hl.model,
                            "cron": hl.cron,
                            "workflow": hl.workflow,
                        }),
                    },
                ));
            }
            runs.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
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
                        "kind": if project.starts_with("hl-") {
                            "headless".to_string()
                        } else {
                            format!("{:?}", e.kind).to_lowercase()
                        },
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
            if let Some((project, e)) = runs
                .iter()
                .find(|(project, e)| format!("{project}:{}", e.ts) == *id)
            {
                print_json(&json!({
                    "id": id,
                    "project": project,
                    "kind": format!("{:?}", e.kind).to_lowercase(),
                    "ts": e.ts,
                    "owner": run_owner(e),
                    "data": e.data,
                }));
                return Ok(());
            }
            // Headless run-log fallback (ADR-0010 Spec E, P10.1): headless
            // runs live in `runs/<run_id>.jsonl`, not the state log.
            if let Some(entry) = run_log::find_run(id) {
                print_json(&json!({
                    "run_id": entry.run_id,
                    "ts": entry.ts,
                    "workflow": entry.workflow,
                    "profile": entry.profile,
                    "harness": entry.harness,
                    "model": entry.model,
                    "mode": entry.mode,
                    "status": entry.status,
                    "tokens_in": entry.tokens_in,
                    "tokens_out": entry.tokens_out,
                    "duration_ms": entry.duration_ms,
                    "prompt_hash": entry.prompt_hash,
                    "response": entry.response,
                    "error": entry.error,
                }));
                return Ok(());
            }
            bail!("run not found: {id} (flywheel runs list | headless run log)")
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
        RunsCmd::Digest {
            since,
            broken_after,
        } => {
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
                Some(c) => runs.iter().filter(|(_, e)| e.ts >= *c).cloned().collect(),
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
            // Headless run logs (ADR-0010 Spec E, P10.1): failures since the
            // cutoff are aggregated separately — they live in `runs/`, not
            // the state log.
            let headless_failed: Vec<Value> = run_log::failures_since(cutoff.as_deref())
                .into_iter()
                .map(|e| {
                    json!({
                        "run_id": e.run_id,
                        "ts": e.ts,
                        "workflow": e.workflow,
                        "profile": e.profile,
                        "status": e.status,
                        "error": e.error,
                    })
                })
                .collect();
            print_json(&json!({
                "since": cutoff,
                "runs": recent.len(),
                "failed": failed,
                "parked": parked,
                "broken_gt_n": broken,
                "headless_failed": headless_failed,
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
/// `flywheel model` — the models.toml registry (ADR-0010 Spec D, P10.2).
fn cmd_models(cmd: &ModelCmd) -> Result<()> {
    match cmd {
        ModelCmd::List { json } => {
            let reg = models::ModelRegistry::load()?;
            let rows: Vec<Value> = reg
                .models
                .iter()
                .map(|(id, e)| {
                    json!({
                        "id": id,
                        "provider": e.provider,
                        "cost_in_per_mtok": e.cost_in_per_mtok,
                        "cost_out_per_mtok": e.cost_out_per_mtok,
                        "caps": e.caps,
                        "fallback": e.fallback,
                    })
                })
                .collect();
            if *json {
                print_json(&json!({ "models": rows }));
            } else if rows.is_empty() {
                println!("no models registered (flywheel model add <id> --provider <p>)");
            } else {
                for r in &rows {
                    println!(
                        "{id:24} {provider:10} ${in_cost:.2}/${out_cost:.2} caps={caps:?} fb={fb:?}",
                        id = r["id"].as_str().unwrap_or(""),
                        provider = r["provider"].as_str().unwrap_or(""),
                        in_cost = r["cost_in_per_mtok"].as_f64().unwrap_or(0.0),
                        out_cost = r["cost_out_per_mtok"].as_f64().unwrap_or(0.0),
                        caps = r["caps"]
                            .as_array()
                            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                            .unwrap_or_default(),
                        fb = r["fallback"].as_str(),
                    );
                }
            }
            Ok(())
        }
        ModelCmd::Show { id } => {
            let reg = models::ModelRegistry::load()?;
            match reg.get(id) {
                Some(e) => print_json(&json!({
                    "id": id,
                    "provider": e.provider,
                    "cost_in_per_mtok": e.cost_in_per_mtok,
                    "cost_out_per_mtok": e.cost_out_per_mtok,
                    "caps": e.caps,
                    "fallback": e.fallback,
                    "effort": e.effort,
                })),
                None => bail!("no model named {id:?} (flywheel model list)"),
            }
            Ok(())
        }
        ModelCmd::Add {
            id,
            provider,
            cost_in_per_mtok,
            cost_out_per_mtok,
            caps,
            fallback,
            effort,
        } => {
            if id.trim().is_empty() {
                bail!("model id cannot be empty");
            }
            let mut reg = models::ModelRegistry::load()?;
            let caps_list: Vec<String> = match caps {
                Some(c) if !c.trim().is_empty() => c
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                _ => vec![],
            };
            reg.models.insert(
                id.clone(),
                models::ModelEntry {
                    provider: provider.clone(),
                    cost_in_per_mtok: *cost_in_per_mtok,
                    cost_out_per_mtok: *cost_out_per_mtok,
                    caps: caps_list,
                    fallback: fallback.clone(),
                    effort: effort.clone(),
                },
            );
            reg.save()?;
            println!("registered model {id} (provider {provider})");
            Ok(())
        }
        ModelCmd::Remove { id } => {
            let mut reg = models::ModelRegistry::load()?;
            if reg.models.remove(id).is_none() {
                bail!("no model named {id:?} (flywheel model list)");
            }
            reg.save()?;
            println!("removed model {id}");
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)] // one param per CLI flag, mirrors cmd_spawn
fn cmd_steer(
    cfg: &Config,
    objective: &str,
    direct: bool,
    crew: Option<&str>,
    project: Option<&str>,
    reuse: Option<&str>,
    retain: bool,
    retire_orphans: bool,
    json: bool,
) -> Result<()> {
    if objective.trim().is_empty() {
        bail!("steer: objective must not be empty");
    }
    // bd-t86p: ONE compose call — the goal workflow (bead-tracked) or its
    // direct arm (non-bead targets: no tracker, no plan approval gate). The
    // delegate is the full `workflow run --execute --mode pane` machinery:
    // lint gate, crew auto-infer, pane pre-spawn, step dispatch via __send,
    // reap-on-end — byte-identical to running `workflow run goal` by hand,
    // with the objective pre-filled.
    let wf_name = if direct { "goal-direct" } else { "goal" };
    let inputs = serde_json::json!({ "objective": objective });
    let run = WorkflowCmd::Run {
        name: wf_name.to_string(),
        inputs: Some(inputs.to_string()),
        crew: crew.map(String::from),
        execute: true,
        mode: "pane".to_string(),
        project: project.map(String::from),
        reuse: reuse.map(String::from),
        retain,
        retire_orphans,
        json,
    };
    cmd_workflow(cfg, &run)
}

/// `profile run --workflow` (bd-6zsl): the invoke command — run the named
/// workflow in pane mode against the spawned agent's workspace (--reuse
/// binds the crew into the SAME workspace; the caller's workspace is never
/// reaped). Pane mode is the only fit: the profile agent is a live pane.
fn profile_workflow_invoke(
    name: &str,
    inputs: Option<&str>,
    workspace: &str,
    project: &str,
) -> WorkflowCmd {
    WorkflowCmd::Run {
        name: name.to_string(),
        inputs: inputs.map(str::to_string),
        crew: None,
        execute: true,
        mode: "pane".to_string(),
        project: Some(project.to_string()),
        reuse: Some(workspace.to_string()),
        retain: false,
        retire_orphans: false,
        json: false,
    }
}

/// Planner arm (bd-u4cu): run the planner profile headlessly via the
/// `__run-headless` leaf and return its response text (the authored workflow
/// YAML). A failed leaf (`ok: false`) or a non-JSON/non-string leaf bails —
/// a broken authoring never reaches the lint gate.
fn planner_author_response(wf_name: &str, prompt: &str) -> Result<String> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__run-headless")
        .arg("planner")
        .arg(prompt)
        .arg("--workflow")
        .arg(wf_name)
        .arg("--json");
    // Same model injection as headless_dispatch — without it the planner
    // harness starts on its default (often wrong / missing).
    if let Ok(cat) = profile::ProfileCatalog::load()
        && let Some(p) = cat.get("planner")
        && !p.model.is_empty()
    {
        cmd.arg("--model").arg(&p.model);
    }
    let output = cmd
        .output()
        .with_context(|| "planner headless authoring run")?;
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow!("planner authoring leaf returned non-JSON: {e}"))?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown leaf error");
        bail!("planner authoring failed: {err}");
    }
    v.get("response")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("planner authoring leaf returned no response"))
}

fn cmd_workflow(cfg: &Config, cmd: &WorkflowCmd) -> Result<()> {
    use toron_workflow::schema::{Action, CrewSpec, Workflow};
    use toron_workflow::{Level, lint_workflow};

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
            execute,
            mode,
            project,
            reuse,
            retain,
            retire_orphans,
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
            // Crew resolution: auto → infer from the workflow (recursing into
            // call_workflow sub-workflows — bd-t86p: the composed fleet);
            // <squad> → explicit role:count. Used for the preview + the
            // engine invoke.
            let workflow_resolver: toron_workflow::WorkflowResolver = Arc::new(|sub: &str| {
                let cat = workflow_catalog::WorkflowCatalog::load().ok()?;
                cat.workflows
                    .get(sub)
                    .cloned()
                    .or_else(|| builtin_workflows::builtin_workflows().get(sub).cloned())
            });
            let crew_plan = match crew.as_deref() {
                None | Some("auto") => crew::plan_crew_with(&wf, &|sub| workflow_resolver(sub)),
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
            // (herdr dispatch is the P4 spawn-await binding). With
            // `--execute --mode headless` the real headless runner replaces
            // it: spawn/dispatch_job steps run through `__run-headless`
            // (ADR-0010 Spec A/E, P10.1). `--execute --mode pane` pre-spawns
            // a real herdr crew from the crew plan and dispatches each step
            // via the loop's `__send` leaf (P10.3, bd-ghcj.1).
            let mut pane_ctx: Option<pane::PaneRunCtx> = None;
            let runner: toron_workflow::ActionRunner = if *execute {
                match mode.as_str() {
                    "headless" => {
                        // bd-omwg: fail closed — a spawn step cannot run
                        // headless (no pane to launch; faking kind-43001/43004
                        // is forbidden, ADR-0010 Spec E). Gate BEFORE invoke:
                        // the engine's run_spawn would return ok:true with no
                        // pane — the exact lie this fixes.
                        headless_spawn_gate(&wf)?;
                        let catalog = profile::ProfileCatalog::load()?;
                        let wf_name = name.clone();
                        let inputs_c = inputs_value.clone();
                        Arc::new(move |a: &Action| {
                            headless_dispatch(&catalog, &wf_name, &inputs_c, None, a)
                        })
                    }
                    "pane" => {
                        let project = project.as_deref().unwrap_or("flywheel");
                        let catalog = profile::ProfileCatalog::load()?;
                        let ctx = pane::pre_spawn_crew(
                            cfg,
                            project,
                            name,
                            &crew_plan,
                            &catalog,
                            reuse.as_deref(),
                            *retain,
                            *retire_orphans,
                        )?;
                        let wf_name = name.clone();
                        let inputs_c = inputs_value.clone();
                        let entries = ctx.entries.clone();
                        let project_c = project.to_string();
                        pane_ctx = Some(ctx);
                        Arc::new(move |a: &Action| {
                            pane::pane_dispatch(
                                &catalog, &project_c, &wf_name, &inputs_c, &entries, a,
                            )
                        })
                    }
                    other => bail!("unknown --mode {other:?} (headless | pane)"),
                }
            } else {
                Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })))
            };
            let rt = tokio::runtime::Runtime::new()?;
            let run_id = format!("fw-cli-{}", state::now_iso());
            // bd-gsvn.3: resolve elastic fan-out refs (`parallel over:
            // "@crew.<role>"`) to concrete counts from the crew plan BEFORE
            // invoke — the engine parses `over` as a number (default 1), so
            // an unresolved @crew.X would silently degrade to one copy.
            let invoke_wf = crew::resolve_crew_fanouts(&wf, &crew_plan);
            // bd-t86p: the goal/direct meta-workflows COMPOSE via
            // call_workflow — inject the catalog+builtin resolver so
            // brainstorm/plan/beads/plan-direct resolve (the cron path got
            // this in bd-8wvz; the workflow-run path must too, else
            // call_workflow returns "unknown workflow" and the steer chain
            // silently no-ops the ideate/plan steps). The same resolver
            // already fed crew inference above.
            let profile_resolver = profile::ProfileCatalog::catalog_profile_resolver();
            // bd-omwg: the SpawnSink drainer. Pane mode keeps the engine alive
            // across invoke — a drainer thread consumes every promise_id the
            // engine pushed (an `await: true` run parks on its job promise
            // until the drainer settles it) and launches a REAL pane per job
            // (profile_launch_args + the pane runner), resolving each promise
            // when the job finishes. The crew workspace stays open until the
            // drainer joins (closing it would kill in-flight spawn panes).
            let mut spawn_receipts: Vec<Value> = vec![];
            let output = match pane_ctx.as_ref() {
                Some(pane_ctx) => {
                    let engine = std::sync::Arc::new(pane::local_engine(
                        &rt,
                        runner,
                        profile_resolver,
                        workflow_resolver,
                    )?);
                    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let drainer = pane::start_spawn_drainer(
                        cfg.clone(),
                        project.as_deref().unwrap_or("flywheel").to_string(),
                        name.clone(),
                        pane_ctx.workspace_id.clone(),
                        engine.clone(),
                        stop.clone(),
                    )?;
                    let out = rt.block_on(async {
                        engine
                            .invoke(&run_id, invoke_wf, inputs_value.clone())
                            .await
                    });
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    spawn_receipts = drainer
                        .join()
                        .map_err(|_| anyhow!("spawn drainer thread panicked"))??;
                    out
                }
                None => rt.block_on(async {
                    let engine = toron_workflow::WorkflowEngine::local_with_resolvers(
                        runner,
                        profile_resolver,
                        workflow_resolver,
                    )?;
                    engine
                        .invoke(&run_id, invoke_wf, inputs_value.clone())
                        .await
                }),
            }?;
            // P10.3 lifecycle: ephemeral default — reap the crew workspace
            // (close it + its panes) at run end unless --retain (keep) or
            // --reuse (the workspace is the caller's).
            let mut reaped = false;
            if let Some(ctx) = &pane_ctx
                && !ctx.retained
            {
                let mut c = Client::connect(Some(&cfg.socket_path()))?;
                match c.workspace_close(&ctx.workspace_id) {
                    Ok(_) => reaped = true,
                    Err(e) => eprintln!(
                        "flywheel: workspace close {} failed: {e:#}",
                        ctx.workspace_id
                    ),
                }
            }
            log(
                "workflow",
                EventKind::Workflow,
                json!({
                    "workflow": name,
                    "status": "done",
                    "crew": crew_plan.members,
                    "run_id": run_id,
                    "mode": mode,
                    "panes": pane_ctx.as_ref().map(|c| c.entries.len()),
                    "reaped": reaped,
                }),
            );
            if *json {
                let mut out = json!({
                    "workflow": name,
                    "run_id": run_id,
                    "crew": crew_plan.members,
                    "mode": mode,
                    "output": output,
                    "spawn": spawn_receipts,
                });
                if let Some(ctx) = &pane_ctx {
                    out["panes"] = json!(ctx.entries);
                    out["workspace_id"] = json!(ctx.workspace_id);
                    out["reaped"] = json!(reaped);
                }
                print_json(&out);
            } else {
                println!(
                    "workflow {name} ran (run_id {run_id}, crew {} member(s), mode {mode})",
                    crew_plan.members.len()
                );
                if let Some(ctx) = &pane_ctx {
                    println!(
                        "  crew: {} pane(s) in workspace {}",
                        ctx.entries.len(),
                        ctx.workspace_id
                    );
                    for e in &ctx.entries {
                        println!(
                            "    {} ({}) [{}]",
                            e.name,
                            e.pane_id,
                            e.profile.as_deref().unwrap_or("-")
                        );
                    }
                }
                if !spawn_receipts.is_empty() {
                    println!("  spawn: {} job(s) launched", spawn_receipts.len());
                    for r in &spawn_receipts {
                        println!(
                            "    {} ({} @{})",
                            r["agent"].as_str().unwrap_or("?"),
                            r["harness"].as_str().unwrap_or("?"),
                            r["profile"].as_str().unwrap_or("?")
                        );
                    }
                }
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
        WorkflowCmd::Compose {
            target,
            base,
            parts,
            agent,
            run,
            project,
            json,
        } => {
            // bd-8xca deterministic arm: assemble from the parts spec.
            // `--agent` (planner arm, bd-u4cu): a headless planner-profile
            // run authors a Workflow YAML from the target + the workflow
            // catalog; the authored workflow goes through the SAME lint→run
            // path as the deterministic assembly. The arms are exclusive.
            if *agent && (!parts.is_empty() || base.is_some()) {
                bail!(
                    "workflow compose: choose one arm — --agent (planner-authored) or --parts/--base (deterministic)"
                );
            }
            if !*agent && parts.is_empty() {
                bail!("workflow compose needs --parts (role:stage or workflow:name) or --agent");
            }
            let all = all_workflows()?;
            let wf_name = format!("composed-{}", state::now_iso().replace([':', '-', '.'], ""));
            let composed = if *agent {
                // Planner arm: author from the target + catalog (workflows,
                // profiles, stages). The planner's response is the ONLY
                // source of the workflow — a failed run or garbage YAML
                // bails below (lint gate), never dispatching.
                let wf_rows: Vec<(&str, &str)> = all
                    .iter()
                    .map(|(n, (w, _))| (n.as_str(), w.description.as_deref().unwrap_or("")))
                    .collect();
                let profile_names = profile::ProfileCatalog::load()?.names();
                let profile_refs: Vec<&str> = profile_names.iter().map(String::as_str).collect();
                let stage_lib = stages::StageLibrary::load()?;
                let stage_rows = stage_lib.list();
                let prompt =
                    crew::planner_authoring_prompt(target, &wf_rows, &profile_refs, &stage_rows);
                let response = planner_author_response(&wf_name, &prompt)?;
                crew::parse_planner_workflow(&response, &wf_name)
                    .map_err(|e| anyhow!("planner-authored workflow invalid: {e}"))?
            } else {
                let base_wf = match base.as_deref() {
                    Some(b) => Some(resolve_workflow(b, &all)?.clone()),
                    None => None,
                };
                let mut parsed: Vec<crew::ComposePart> = Vec::new();
                for token in parts {
                    parsed.push(crew::parse_compose_part(token).map_err(anyhow::Error::msg)?);
                }
                crew::compose_workflow(&wf_name, target, base_wf.as_ref(), &parsed)
            };

            // Fail-stop gate: lint before run / emit.
            let blocking = lint_one(&wf_name, &composed);
            if blocking > 0 {
                bail!(
                    "composed workflow {wf_name:?} has {blocking} blocking lint issue(s) — refusing to run"
                );
            }

            // The crew plan (auto-inferred from the composed parts) — the
            // captain's fleet preview + the run's spawn.
            let resolve = |sub: &str| all.get(sub).map(|(w, _)| w.clone());
            let crew_plan = crew::plan_crew_with(&composed, &resolve);

            if !*run {
                if *json {
                    print_json(&json!({
                        "workflow": wf_name,
                        "target": target,
                        "crew": crew_plan.members,
                        "composed": &composed,
                    }));
                } else {
                    println!("composed workflow {wf_name} (lint clean):");
                    for s in &composed.steps {
                        println!(
                            "  - {} ({})",
                            s.name.as_deref().unwrap_or("<unnamed>"),
                            match &s.action {
                                Action::DispatchJob { spec, .. } => spec
                                    .get("profile")
                                    .and_then(Value::as_str)
                                    .unwrap_or("dispatch"),
                                Action::CallWorkflow { workflow, .. } => workflow.as_str(),
                                other => other.type_name(),
                            }
                        );
                    }
                    print_json(&json!({
                        "workflow": wf_name,
                        "target": target,
                        "crew": crew_plan.members,
                    }));
                }
                return Ok(());
            }

            // Run the composed workflow: the same pane machinery as
            // steer/workflow run — lint passed, spawn the inferred crew +
            // dispatch each step via __send, reap on end. The composed struct
            // is invoked directly (the engine takes ANY Workflow).
            let project_name = project.as_deref().unwrap_or("flywheel");
            let catalog = profile::ProfileCatalog::load()?;
            let ctx = pane::pre_spawn_crew(
                cfg,
                project_name,
                &wf_name,
                &crew_plan,
                &catalog,
                None,
                false,
                true,
            )?;
            let entries = ctx.entries.clone();
            let project_c = project_name.to_string();
            let wf_name_c = wf_name.clone();
            let inputs_c = json!({ "objective": target });
            let runner: toron_workflow::ActionRunner = {
                let entries = entries.clone();
                let inputs_c = inputs_c.clone();
                Arc::new(move |a: &Action| {
                    pane::pane_dispatch(&catalog, &project_c, &wf_name_c, &inputs_c, &entries, a)
                })
            };
            let rt = tokio::runtime::Runtime::new()?;
            let run_id = format!("fw-composed-{}", state::now_iso());
            let invoke_wf = crew::resolve_crew_fanouts(&composed, &crew_plan);
            let workflow_resolver: toron_workflow::WorkflowResolver = Arc::new(|sub: &str| {
                let cat = workflow_catalog::WorkflowCatalog::load().ok()?;
                cat.workflows
                    .get(sub)
                    .cloned()
                    .or_else(|| builtin_workflows::builtin_workflows().get(sub).cloned())
            });
            let profile_resolver = profile::ProfileCatalog::catalog_profile_resolver();
            // bd-omwg: the SpawnSink drainer — pane mode keeps the engine
            // alive across invoke so the drainer thread can launch a real
            // pane per spawned job and settle await:true promises.
            let mut spawn_receipts: Vec<Value> = vec![];
            let engine = std::sync::Arc::new(pane::local_engine(
                &rt,
                runner,
                profile_resolver,
                workflow_resolver,
            )?);
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let drainer = pane::start_spawn_drainer(
                cfg.clone(),
                project_name.to_string(),
                wf_name.clone(),
                ctx.workspace_id.clone(),
                engine.clone(),
                stop.clone(),
            )?;
            let output =
                rt.block_on(async { engine.invoke(&run_id, invoke_wf, inputs_c.clone()).await })?;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            spawn_receipts = drainer
                .join()
                .map_err(|_| anyhow!("spawn drainer thread panicked"))??;
            let mut reaped = false;
            if !ctx.retained {
                let mut c = Client::connect(Some(&cfg.socket_path()))?;
                match c.workspace_close(&ctx.workspace_id) {
                    Ok(_) => reaped = true,
                    Err(e) => eprintln!(
                        "flywheel: workspace close {} failed: {e:#}",
                        ctx.workspace_id
                    ),
                }
            }
            log(
                "workflow",
                EventKind::Workflow,
                json!({
                    "workflow": wf_name,
                    "status": "done",
                    "crew": crew_plan.members,
                    "run_id": run_id,
                    "mode": "pane",
                    "panes": entries.len(),
                    "reaped": reaped,
                }),
            );
            if *json {
                print_json(&json!({
                    "workflow": wf_name,
                    "run_id": run_id,
                    "crew": crew_plan.members,
                    "mode": "pane",
                    "output": output,
                    "spawn": spawn_receipts,
                    "workspace_id": ctx.workspace_id,
                    "reaped": reaped,
                }));
            } else {
                println!(
                    "composed workflow {wf_name} ran (run_id {run_id}, crew {} member(s), mode pane)",
                    crew_plan.members.len()
                );
                if !spawn_receipts.is_empty() {
                    println!("  spawn: {} job(s) launched", spawn_receipts.len());
                }
                print_json(&output);
            }
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
            let resolve = |sub: &str| all.get(sub).map(|(w, _)| w.clone());
            let plan = crew::plan_crew_with(wf, &resolve);
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
        WorkflowCmd::Remove { name } => {
            let all = all_workflows()?;
            match all.get(name).map(|(_, s)| *s) {
                Some("builtin") => bail!(
                    "cannot remove built-in workflow {name:?} — override it via a catalog file of the same name in ~/.config/flywheel/workflows/"
                ),
                Some(_) => {
                    let path = config::config_dir()
                        .join("workflows")
                        .join(format!("{name}.yaml"));
                    std::fs::remove_file(&path)
                        .with_context(|| format!("remove {}", path.display()))?;
                    println!("removed workflow {name} ({})", path.display());
                }
                None => bail!("no workflow named {name:?}"),
            }
            Ok(())
        }
    }
}

/// `flywheel cron` — the standalone cron surface (ADR-0012 Spec 1/4,
/// P12.3). Crons are YAML files in three tiers (per-project > global >
/// built-in) with a shared CronSpec body on the toron-workflow engine.
/// Status is derived from the state log + run log.
fn cmd_cron(cfg: &Config, cmd: &CronCmd) -> Result<()> {
    use toron_workflow::lint::{Level, lint_cron};

    /// Lint one cron: engine tier (schedule parse, policy sanity, step/graph
    /// checks — P1) + flywheel tier (owner-profile resolution against the
    /// ProfileCatalog). Returns the blocking (Error-level) count.
    fn lint_one(name: &str, spec: &toron_workflow::schema::CronSpec) -> usize {
        let report = lint_cron(spec);
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
        // Flywheel tier: the ENGINE cannot resolve owner profiles (no
        // dependency inversion — it never imports flywheel's
        // ProfileCatalog). Owned here, fail-stop (ADR-0012: cron lint
        // validates owner-profile resolution).
        let profiles = profile::ProfileCatalog::load().unwrap_or_default();
        let owner = spec.owner.trim_start_matches('@');
        if !owner.is_empty() && profiles.get(owner).is_none() {
            eprintln!(
                "[{name}]    error owner profile {owner:?} does not resolve (known: {})",
                profiles.names().join(", ")
            );
            blocking += 1;
        }
        blocking
    }

    /// The resolved cron by name (fail-stop on unknown).
    fn resolve<'a>(rows: &'a [cron::CronRow], name: &str) -> Result<&'a cron::CronRow> {
        rows.iter()
            .find(|r| r.name() == name)
            .ok_or_else(|| anyhow!("no cron named {name:?} (flywheel cron list)"))
    }

    match cmd {
        CronCmd::List { json, project } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            if rows.is_empty() {
                println!("no crons defined");
                return Ok(());
            }
            let mut rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let (last, in_flight) = cron::last_run_state(row.name());
                    let status = cron::derive_status(row.enabled(), in_flight, last.as_deref());
                    let next_due = cron::next_due(row.cron(), cron::now_ts());
                    cron::cron_row_json(row, status, next_due)
                })
                .collect();
            rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            if *json {
                print_json(&json!({ "crons": rows }));
            } else {
                println!(
                    "{:<28} {:<22} {:<16} {:<7} {:<8} {:<9} {:<6} {:<7}",
                    "NAME", "OWNER", "SCHEDULE", "SCOPE", "SOURCE", "STATUS", "STEPS", "STATE"
                );
                for row in &rows {
                    println!(
                        "{:<28} {:<22} {:<16} {:<7} {:<8} {:<9} {:<6} {}",
                        row["name"].as_str().unwrap_or(""),
                        row["owner"].as_str().unwrap_or(""),
                        row["schedule"].as_str().unwrap_or(""),
                        row["scope"].as_str().unwrap_or(""),
                        row["source"].as_str().unwrap_or(""),
                        row["status"].as_str().unwrap_or(""),
                        row["steps"].as_i64().unwrap_or(0),
                        if row["enabled"].as_bool().unwrap_or(false) {
                            "on"
                        } else {
                            "paused"
                        },
                    );
                }
            }
            Ok(())
        }
        CronCmd::Show {
            name,
            json,
            project,
        } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            let row = resolve(&rows, name)?;
            let (last, in_flight) = cron::last_run_state(name);
            let status = cron::derive_status(row.enabled(), in_flight, last.as_deref());
            let next_due = cron::next_due(row.cron(), cron::now_ts());
            if *json {
                print_json(&cron::cron_row_json(row, status, next_due));
            } else {
                println!("name:     {}", row.name());
                println!("schedule: {}", row.cron());
                println!("owner:    {}", row.owner());
                println!(
                    "scope:    {}",
                    match row.scope() {
                        toron_workflow::schema::CronScope::Host => "host",
                        toron_workflow::schema::CronScope::Project => "project",
                    }
                );
                println!("source:   {}", row.source_label());
                println!(
                    "mode:     {}",
                    match row.spec.mode {
                        toron_workflow::schema::CronMode::Headless => "headless",
                        toron_workflow::schema::CronMode::Pane => "pane",
                    }
                );
                println!("status:   {status}");
                if let Some(d) = next_due {
                    println!("next_due: {d}");
                }
                if let Some(p) = &row.spec.policy {
                    println!("policy:   {p:?}");
                }
                if let Some(d) = &row.spec.description {
                    println!("desc:     {d}");
                }
                for (i, s) in row.spec.steps.iter().enumerate() {
                    let n = s.name.clone().unwrap_or_else(|| "<unnamed>".to_string());
                    println!("step {i}:   {n} ({})", s.action.type_name());
                }
            }
            Ok(())
        }
        CronCmd::Lint { name, project } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            match name {
                Some(n) => {
                    let row = resolve(&rows, n)?;
                    let blocking = lint_one(n, &row.spec);
                    if blocking > 0 {
                        std::process::exit(1);
                    }
                    println!("cron {n} lints clean (source {})", row.source_label());
                    Ok(())
                }
                None => {
                    let mut blocking_total = 0;
                    for row in &rows {
                        blocking_total += lint_one(row.name(), &row.spec);
                    }
                    if blocking_total > 0 {
                        std::process::exit(1);
                    }
                    println!("lint clean: {} cron(s)", rows.len());
                    Ok(())
                }
            }
        }
        CronCmd::Run {
            name,
            project,
            json,
        } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            let row = resolve(&rows, name)?.clone();
            let source = row.source_label().to_string();
            // Fail-stop gate: lint before running (ADR-0012 — run is for
            // testing; a cron that does not lint cannot run).
            let blocking = lint_one(name, &row.spec);
            if blocking > 0 {
                bail!("cron {name:?} has {blocking} blocking lint issue(s) — refusing to run");
            }
            let spec = row.spec;
            // The tick body: the SAME spec-derived workflow the durable loop
            // fires (one engine, two input types — ADR-0012 Spec 2).
            let wf = toron_workflow::cron::spec_tick_workflow(&spec);
            // bd-8wvz: on-demand `cron run` is a structural DRY-RUN of the
            // body, never a live agent dispatch. The production durable loop
            // (start_loops -> spec_runner -> headless_dispatch) owns real
            // dispatch; here the recording runner completes immediately and
            // prints the step-output map so the run is always visible (never
            // exit 0 with zero output).
            let runner = cron::recording_runner();
            let rt = tokio::runtime::Runtime::new()?;
            let run_id = format!("cron-run-{}", state::now_iso());
            // ADR-0012: the cron body composes workflows via call_workflow —
            // resolve them against the flywheel catalog + built-ins.
            let workflow_resolver: toron_workflow::WorkflowResolver = Arc::new(|name: &str| {
                let cat = workflow_catalog::WorkflowCatalog::load().ok()?;
                cat.workflows
                    .get(name)
                    .cloned()
                    .or_else(|| builtin_workflows::builtin_workflows().get(name).cloned())
            });
            let profile_resolver = profile::ProfileCatalog::catalog_profile_resolver();
            // bd-8wvz: even a dry-run must never hang — a body with
            // `delay`/`request_approval`/`spawn` steps parks on a promise the
            // recording runner cannot resolve. 25s cap → explicit timeout
            // error, never a silent never-returning command.
            let output = rt.block_on(async {
                let engine = toron_workflow::WorkflowEngine::local_with_resolvers(
                    runner,
                    profile_resolver,
                    workflow_resolver,
                )?;
                tokio::time::timeout(
                    std::time::Duration::from_secs(25),
                    engine.invoke(&run_id, wf, json!({})),
                )
                .await
                .map_err(|_elapsed| {
                    anyhow::anyhow!(
                        "cron {name} on-demand dry-run timed out after 25s (a delay/approval/spawn step parks the body — the scheduled durable-loop path is the real execution)"
                    )
                })?
            })?;
            log(
                "workflow",
                EventKind::Workflow,
                json!({
                    "workflow": name,
                    "status": "done",
                    "run_id": run_id,
                    "cron": name,
                }),
            );
            if *json {
                print_json(&json!({
                    "cron": name,
                    "run_id": run_id,
                    "source": source,
                    "dry_run": true,
                    "output": output,
                }));
            } else {
                println!(
                    "cron {name} dry-run ok (run_id {run_id}, source {source}) — no live dispatch; recorded steps:"
                );
                print_json(&output);
            }
            Ok(())
        }
        CronCmd::Pause { name, project } => {
            cron::set_cron_enabled(cfg, project.as_deref(), name, false)?;
            println!("paused cron {name}");
            Ok(())
        }
        CronCmd::Resume { name, project } => {
            cron::set_cron_enabled(cfg, project.as_deref(), name, true)?;
            println!("resumed cron {name}");
            Ok(())
        }
        CronCmd::Status { json, project } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            if rows.is_empty() {
                println!("no crons defined");
                return Ok(());
            }
            let mut rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let (last, in_flight) = cron::last_run_state(row.name());
                    let status = cron::derive_status(row.enabled(), in_flight, last.as_deref());
                    let next_due = cron::next_due(row.cron(), cron::now_ts());
                    json!({
                        "name": row.name(),
                        "status": status,
                        "enabled": row.enabled(),
                        "next_due": next_due,
                        "source": row.source_label(),
                    })
                })
                .collect();
            rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            if *json {
                print_json(&json!({ "crons": rows }));
            } else {
                println!(
                    "{:<28} {:<9} {:<8} {:<12} {:<8}",
                    "NAME", "STATUS", "STATE", "NEXT_DUE", "SOURCE"
                );
                for row in &rows {
                    println!(
                        "{:<28} {:<9} {:<8} {} {:<8}",
                        row["name"].as_str().unwrap_or(""),
                        row["status"].as_str().unwrap_or(""),
                        if row["enabled"].as_bool().unwrap_or(false) {
                            "on"
                        } else {
                            "paused"
                        },
                        row["next_due"].as_i64().unwrap_or(0),
                        row["source"].as_str().unwrap_or(""),
                    );
                }
            }
            Ok(())
        }
        CronCmd::History { name, json } => {
            let runs = cron::cron_history(name);
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
        CronCmd::Start { project } => {
            // Blocks until Ctrl-C: durable loops + policy-sink drain (bead
            // side-effects). Run under the project dir so `br create` lands
            // in the right .beads.
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cron::start_loops(cfg, project.as_deref()))
        }
        CronCmd::Remove { name, project } => {
            cron::remove_cron(cfg, project.as_deref(), name)?;
            println!("removed cron {name}");
            Ok(())
        }
    }
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
            if let Some(stage) = spec.get("stage").and_then(|v| v.as_str())
                && !stages.has(stage)
            {
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

/// Resolve a pane id in the default project workspace for `profile run` /
/// `squad spawn`: reuse the first workspace, else create one, then return a
/// PANE id inside it — `agent_start` takes a pane id, never a workspace id
/// (bd-ude8: herdr rejects a workspace id with `agent_pane_not_found`).
fn resolved_spec_workspace(
    _cfg: &Config,
    backend: &dyn backend::MultiplexerBackend,
    _harness: &str,
) -> Result<String> {
    let ws_id = match backend.workspace_list()?.first() {
        Some(first) => first.workspace_id.clone(),
        None => backend.workspace_create(None)?.workspace_id,
    };
    let panes = backend.pane_list(Some(&ws_id))?;
    panes
        .first()
        .map(|p| p.pane_id.clone())
        .ok_or_else(|| anyhow::anyhow!("no pane in workspace {ws_id} for profile spawn"))
}

/// Merge one entry into the project's spawn registry (bd-l2ky): preserve
/// entries for other agents, replace the same name. Shared by `profile run`
/// and `squad spawn` so `__send --profile` and loop conditioning resolve
/// the agent's profile (bd-gsvn.1). Best-effort — a registry failure is not
/// a spawn failure (cmd_spawn precedent).
pub(crate) fn record_spawn_entry(cfg: &Config, project: &str, entry: state::SpawnEntry) {
    let key = match project_dir_str(cfg, project) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("flywheel: cannot record spawn entry for {project}: {e:#}");
            return;
        }
    };
    let mut registry = state::load_spawn_registry(&key);
    registry.retain(|e| e.name != entry.name);
    registry.push(entry);
    if let Err(e) = state::save_spawn_registry(&key, &registry) {
        eprintln!("flywheel: cannot save spawn registry: {e:#}");
    }
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

    let mail_target = |agent: &str| resolve_mail_target_registry(&project, agent).to_string();

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
        // harvest the speaker's latest message in the thread — bails loudly
        // when no reply actually landed, so "replied" prints only after the
        // thread moved (bd-debate-reply-routing-99ad)
        last_reply = Some(harvest_latest(
            mail.as_ref(),
            &key,
            &thread,
            &resolve_mail_target(speaker),
        )?);
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
    println!("debate {thread} verdict: {verdict}");
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

    let mail_target = |agent: &str| resolve_mail_target_registry(project, agent);
    let subject = format!("[plan-review] {project}");

    eprintln!(
        "plan-review {thread}: planner={} reviewer={} implementer={} max_change_rounds={max_rounds} paper-first",
        planner, reviewer, implementer
    );

    // --- plan ---
    let mut change_rounds = 0u32;
    let mut current_path = plan_path.clone();

    let open = pri_planner_prompt(feature, &current_path);
    mail_send(
        &*mail,
        &key,
        &[mail_target(&planner)],
        &subject,
        &open,
        "high",
        true,
        Some(&thread),
    )?;
    settle_agent(&*mux, &planner)?;
    let planner_mail = mail_target(&planner);
    let plan_reply = harvest_latest(mail.as_ref(), &key, &thread, &planner_mail)?;
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
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&planner)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(&*mux, &planner)?;
                let pr = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&planner))?;
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
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&reviewer)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(&*mux, &reviewer)?;
                let body = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&reviewer))?;
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
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&implementer)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
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
                mail_send(
                    &*mail,
                    &key,
                    &[mail_target(&reviewer)],
                    &subject,
                    &msg,
                    "high",
                    true,
                    Some(&thread),
                )?;
                settle_agent(&*mux, &reviewer)?;
                let rev2 = harvest_latest(mail.as_ref(), &key, &thread, &mail_target(&reviewer))?;
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

/// Pure selection of the newest in-thread row from `sender` over
/// inbox-projection rows (the FTS-free path shared by harvest + reply
/// verification). Matches the sender NAME (toron resolves registered npubs;
/// agent-mail returns `from` directly) or the raw npub hex (toron's
/// fallback when the sender never registered). Newest wins.
///
/// governed-by: ADR-0001 §9.1 (bd-debate-harvest-dsww / bd-debate-reply-routing-99ad)
fn newest_in_thread<'a>(
    items: &'a [backend::MailItem],
    thread: &str,
    sender: &str,
    sender_npub: &str,
) -> Option<&'a backend::MailItem> {
    items
        .iter()
        .filter(|m| m.thread_id.as_ref().map(|t| t.as_str()) == Some(thread))
        .filter(|m| m.sender == sender || (!sender_npub.is_empty() && m.sender == sender_npub))
        .max_by_key(|m| m.created_ts_us)
}

/// Harvest the newest in-thread reply from `sender` via the INBOX
/// PROJECTION (fetch_inbox), not FTS search — hyphenated thread ids like
/// `debate-20260806T130548` are not reliably phrase-searchable in FTS5
/// (index-coverage gap, cf bd-debate-harvest-dsww), so a fresh thread can
/// be invisible to `search_messages` while the reply sits in the inbox.
/// The orchestrator's own inbox carries every in-thread message with its
/// exact `thread_id` — match client-side, newest wins.
///
/// Inbox rows carry the sender NAME (toron resolves registered npubs;
/// agent-mail returns `from` directly) or the raw npub hex — whois
/// resolves the name so both match. Fails loudly (bail) when the thread
/// has no readable reply from `sender`: never ships an empty counter or
/// verdict prompt.
///
/// review-fix F1: the debate/plan-review patterns need the reply BODY
/// (the argument / the APPROVE|CHANGES: token), not the subject line.
///
/// governed-by: ADR-0001 §9.1/§9.2 (bd-debate-harvest-dsww)
fn harvest_latest(
    mail: &dyn backend::MailSurface,
    key: &str,
    thread: &str,
    sender: &str,
) -> Result<String> {
    let sender_npub = backend::run_mail(mail.whois(key, sender))
        .map(|w| w.npub)
        .unwrap_or_default();
    // limit: None — the inbox projects oldest-first (G7 rumor time), so a
    // bounded `take` would keep the OLDEST rows and could drop a fresh
    // debate thread entirely. Full projection, client-side filter.
    let items = backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key.to_string(),
        agent: mail.agent_name().to_string(),
        unread_only: false,
        include_bodies: true,
        limit: None,
        urgent_only: false,
        since_ts: None,
    }))?;
    let body = newest_in_thread(&items, thread, sender, &sender_npub)
        .map(|m| {
            // body_md first, then subject — mirrors the precedence in
            // extract_xproj_from_inbox so a body-bearing envelope wins.
            if m.body_md.is_empty() {
                m.subject.trim().to_string()
            } else {
                m.body_md.trim().to_string()
            }
        })
        .filter(|s| !s.is_empty());
    match body {
        Some(b) => Ok(b),
        None => bail!(
            "harvest_latest: no in-thread reply from {sender} in thread {thread} \
             (inbox projection has no matching row) — the reply never landed, \
             do not ship an empty prompt"
        ),
    }
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
         \x20 flywheel loop run <project> --queue p1,p2,p4,p5,p6,p8\n\
         \n\
         (kills crash-safe: restart the same command to resume;\n\
         \x20 flywheel loop status <project> to inspect)\n\
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
         \x20 - flywheel mail locks reserve <project> --paths src/foo.rs --exclusive\n\
         \x20 - flywheel mail locks list <project>      who holds what (lock board)\n\
         \x20 - flywheel mail unlock <project>          release your locks\n\n\
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
        AgentStatusChanged {
            pane,
            agent,
            status,
        } => (
            "pane.agent_status_changed",
            pane.0.clone(),
            agent.clone(),
            status.as_str().to_string(),
        ),
        AgentDetected {
            pane,
            agent,
            status,
        } => (
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

/// Dispatch options for the `__send` internal leaf (`cmd_internal_send`):
/// confirm-wait, resume-dedup invocation/iteration, and profile conditioning.
/// Bundled to keep the leaf's signature under clippy's argument cap; the
/// core `cfg`/`project`/`name`/`text` params stay positional.
struct SendOptions<'a> {
    confirm_ms: Option<u64>,
    invocation: Option<&'a str>,
    iteration: Option<u64>,
    profile: Option<&'a str>,
}

/// bd-eign.10: build the skill-load prefix for a profiled agent's FIRST
/// dispatch — a single line listing the profile's skills via the skill://
/// scheme (the harness resolves them). `None` when the agent already got it,
/// has no profile, or the profile lists no skills.
fn skill_injection_prefix(entry: &state::SpawnEntry, skills: &[String]) -> Option<String> {
    if entry.profile.is_none() || entry.skills_injected || skills.is_empty() {
        return None;
    }
    let paths = skills
        .iter()
        .map(|s| format!("skill://{s}"))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!("Read {paths} before starting.\n\n"))
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
    opts: SendOptions<'_>,
) -> Result<()> {
    let SendOptions {
        confirm_ms,
        invocation,
        iteration,
        profile,
    } = opts;
    // bd-gsvn.1: Spec C role conditioning lands in __send — the ONE dispatch
    // leaf shared by the pane runner and both loop paths. When a profile is
    // given, the behavior preamble is prepended (conditioned_prompt) and the
    // send is logged to runs/ (mode: pane). No profile = raw text, exactly
    // the pre-migration behavior (plain swarm sends byte-identical).
    let profile_name = profile.filter(|p| !p.is_empty());
    let profile_obj = match profile_name {
        Some(p) => {
            let catalog = profile::ProfileCatalog::load()?;
            match catalog.get(p) {
                Some(pr) => Some((
                    p.to_string(),
                    pr.harness.clone(),
                    pr.model.clone(),
                    pr.behavior.clone(),
                    pr.skills.clone(),
                )),
                None => {
                    eprintln!(
                        "flywheel: __send --profile {p} unknown (flywheel profile list) — sending raw"
                    );
                    None
                }
            }
        }
        None => None,
    };
    let started = std::time::Instant::now();

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

    // bd-eign.10: skill-load prefix on the FIRST real dispatch to a profiled
    // agent — after the dedup gate, so a skipped (deduped) send never marks
    // the prefix delivered. Registry-tracked (skills_injected), advisory:
    // a lost update under concurrent __send subprocesses repeats the prefix
    // once at worst.
    let key = project_dir_str(cfg, project)?;
    let mut send_base = text.to_string();
    if let Some((_, _, _, _, skills)) = &profile_obj
        && !skills.is_empty()
    {
        let mut entries = state::load_spawn_registry(&key);
        let injected = entries
            .iter_mut()
            .find(|e| e.name == name)
            .map(|entry| {
                if let Some(prefix) = skill_injection_prefix(entry, skills) {
                    send_base = format!("{prefix}{send_base}");
                    entry.skills_injected = true;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if injected && let Err(e) = state::save_spawn_registry(project, &entries) {
            eprintln!("flywheel: cannot persist skills_injected for {name}: {e:#}");
        }
    }
    let send_text = match &profile_obj {
        Some((_, _, _, behavior, _)) => headless::conditioned_prompt(behavior, &send_base),
        None => send_base,
    };

    // ADR-0019 Spec 2: a dispatched bead brief (behavior + skills prefix +
    // brief) over the 4 KiB cap is a LOOP BUG — fail loud, never silent
    // truncation. Only briefs are capped (first line `GOAL ...`); captain
    // steers, nudges, and mail wakes are raw text and exempt.
    if text.starts_with("GOAL ") && send_text.len() > durable::DISPATCH_PROMPT_CAP {
        bail!(
            "flywheel: dispatched prompt for bead {} is {} bytes — over the {} byte cap (loop bug; NOT sent, never truncated)",
            durable::bead_id_from_prompt(text).unwrap_or("<unknown>"),
            send_text.len(),
            durable::DISPATCH_PROMPT_CAP,
        );
    }

    // review-fix F6: __send routes through the backend factory (ADR-0002).
    // The trait now exposes agent_prompt_coded with landing data, so the
    // durable loop's dispatch leaf is backend-agnostic (herdr today; toron /
    // superlogical later) with byte-identical landing classification.
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let mut agent = backend
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name));
    if agent.is_none() {
        // bd-fw-name-binding-loss-7rc auto-heal: an agent that restarted in
        // its pane lost its name binding (name:null) and name resolution
        // misses — restore from the spawn registry, then re-resolve.
        if rebind_from_registry(&*backend, project, name, &ws_id)? {
            agent = backend
                .agent_list()?
                .into_iter()
                .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name));
        }
    }
    let agent = agent.ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
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
        // prompt was written to the agent's terminal. bd-4pxy: a booting
        // agent rejects name-addressed prompts — retry once via the pane id.
        match backend.prompt(&target, &send_text, None) {
            Ok(_) => {}
            Err(e) if is_agent_not_ready(&e.code) => {
                if let Some(pane) = not_ready_retry_target(&target, &agent, false) {
                    eprintln!("flywheel: {name} agent_not_ready — retrying via pane id {pane}");
                    backend.prompt(&pane, &send_text, None)?;
                } else {
                    return Err(e.into());
                }
            }
            Err(e) => return Err(e.into()),
        }
        record(true, false);
        if let Some((pname, _, _, _, _)) = &profile_obj {
            log_engine_dispatch(project, pname, text, &send_text, started, true);
        }
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
    // bd-4pxy: `agent_not_ready` (a booting agent) → retry once via the
    // pane id before classifying.
    let landing = backend.prompt_coded(&target, &send_text, Some(&wait))?;
    let landing = if landing.code.as_deref() == Some("agent_not_ready")
        && let Some(pane) = not_ready_retry_target(&target, &agent, false)
    {
        eprintln!("flywheel: {name} agent_not_ready — retrying via pane id {pane}");
        backend.prompt_coded(&pane, &send_text, Some(&wait))?
    } else {
        landing
    };
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
    if let Some((pname, _, _, _, _)) = &profile_obj {
        log_engine_dispatch(project, pname, text, &send_text, started, landed);
    }
    print_json(&json!({
        "ok": true, "name": name, "landed": landed,
        "status": status, "reason": reason,
    }));
    Ok(())
}

/// bd-gsvn.1: write the runs/ entry for a profile-conditioned loop/pane
/// dispatch (mode: pane — the engine's pane-runner logging gain). The
/// entry's `prompt_hash` covers the CONDITIONED text (what the agent
/// actually received); `model`/`harness` come from the profile.
fn log_engine_dispatch(
    project: &str,
    profile_name: &str,
    raw_text: &str,
    sent_text: &str,
    started: std::time::Instant,
    landed: bool,
) {
    use sha1::Digest;
    let _ = project;
    let mut hasher = sha1::Sha1::new();
    sha1::Digest::update(&mut hasher, sent_text.as_bytes());
    let prompt_hash = hex::encode(sha1::Digest::finalize(hasher));
    let (harness, model) = profile::ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.get(profile_name).cloned())
        .map(|p| (p.harness, p.model))
        .unwrap_or_default();
    let entry = run_log::RunLogEntry {
        run_id: run_log::new_run_id(),
        ts: state::now_iso(),
        workflow: String::new(),
        cron: None,
        profile: profile_name.to_string(),
        harness,
        model,
        mode: "pane".into(),
        prompt_hash,
        response: String::new(),
        status: if landed {
            "success".into()
        } else {
            "fail".into()
        },
        tokens_in: 0,
        tokens_out: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        exit_code: if landed { 0 } else { 1 },
        error: None,
    };
    if let Err(e) = run_log::write_run(&entry) {
        eprintln!("flywheel: cannot write run log for @{profile_name}: {e:#}");
    }
    let _ = raw_text;
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
    if regex { "regex" } else { "match" }
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
    let beads = load_ready_beads(cfg, project)?;
    let beads: Vec<Value> = beads
        .iter()
        // ADR-0019 (bd-w1-verify-fence): emit the parsed VERIFY command so
        // the durable `parse_bead_list` consumer carries it too.
        .map(|b| json!({ "id": b.id, "title": b.title, "verify": b.verify }))
        .collect();
    print_json(&json!({ "beads": beads }));
    Ok(())
}

/// internal leaf: in-progress beads as [{id, title, assignee}] via
/// `br list --status in_progress --json` (bd-627z.1: assignee drives the
/// stranded-reclaim + scoped-completion decision).
fn cmd_internal_inprogress(cfg: &Config, project: &str) -> Result<()> {
    let beads = load_inprogress_beads(cfg, project)?;
    let beads: Vec<Value> = beads
        .iter()
        .map(
            |b| json!({ "id": b.id, "title": b.title, "assignee": b.assignee, "verify": b.verify }),
        )
        .collect();
    print_json(&json!({ "beads": beads }));
    Ok(())
}

/// internal leaf (bd-627z.1, bd-qk9q): re-open stranded in_progress beads to
/// ready. A bead is stranded when its assignee is NOT one of the `--live`
/// agent names (captain, dead agent, unknown identity) — it would stall the
/// batch forever. An in_progress bead with NO assignee is stranded too
/// (bd-qk9q: nobody holds it, no agent works it, nothing else closes it —
/// the bd-bvwf.1 strand). Presence-based, never status-based: an assignee in
/// the live list is live regardless of status (bd-bvwf.5: no reclaim of
/// unknown-but-present agents). Reclaim = `br update <id> --status open
/// --assignee ""` so the bead returns to the ready pool and the loop
/// re-dispatches it.
///
/// Emits `{"reclaimed": [ids], "failed": [{"id", "error"}]}` — the durable
/// loop re-dispatches reclaimed ids this poll.
fn cmd_internal_reclaim_stranded(cfg: &Config, project: &str, live: &str) -> Result<()> {
    let live_names: Vec<String> = live
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let beads = load_inprogress_beads(cfg, project)?;
    let stranded: Vec<durable::ReadyBead> = beads
        .into_iter()
        .filter(|b| match &b.assignee {
            Some(a) => !live_names.iter().any(|l| l == a),
            None => true,
        })
        .collect();
    let dir = project_dir(cfg, project);
    let mut reclaimed: Vec<String> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    for b in &stranded {
        let out = std::process::Command::new("br")
            .args(["update", &b.id, "--status", "open", "--assignee", ""])
            .current_dir(&dir)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                eprintln!(
                    "flywheel: reclaimed stranded bead {} (assignee {:?})",
                    b.id, b.assignee
                );
                reclaimed.push(b.id.clone());
            }
            Ok(o) => failed.push(json!({
                "id": b.id,
                "error": String::from_utf8_lossy(&o.stderr).trim().to_string(),
            })),
            Err(e) => failed.push(json!({ "id": b.id, "error": format!("br not found: {e}") })),
        }
    }
    print_json(&json!({ "reclaimed": reclaimed, "failed": failed }));
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

/// internal leaf: the profile name an agent serves, from the spawn registry
/// (bd-gsvn.1). Empty line when the agent has no profile (plain swarm
/// spawns — byte-identical pre-migration behavior). Registry read only —
/// never consults herdr.
fn cmd_internal_profile(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let dir = project_dir(cfg, project);
    let key = match dir.to_str() {
        Some(k) => k.to_string(),
        None => return Ok(()),
    };
    let registry = state::load_spawn_registry(&key);
    let profile = registry
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.profile.clone())
        .unwrap_or_default();
    println!("{profile}");
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
    // bd-harness-alt-screen-reads (H4): route through the alt-screen ladder —
    // recent-unwrapped first (wire source `recent_unwrapped`), bounded wait
    // for idle + retry on agent_not_idle, visible fallback, then the docs'
    // file-path fallback — so the loop's context guard still sees
    // CONTEXT_LIMIT markers mid-task.
    let mut ops = herdr_shim::BackendAltScreenRead::new(&*backend, &target, 60);
    let text = match alt_screen_read_ladder(&mut ops, herdr_shim::RECENT_UNWRAPPED_SOURCE) {
        Ok(t) => t,
        Err(e) => return Err(anyhow!("{target}: alt-screen read failed: {e:#}")),
    };
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
/// under the same name (fresh session context). bd-ude8: omp-in-herdr
/// EXITS on PTY stdin EOF, and `pane.close` on its pane delivers exactly
/// that — so omp restarts IN PLACE (same pane, no split, no close); other
/// kinds keep the split+close cycle.
fn cmd_internal_respawn(
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
    force: bool,
) -> Result<()> {
    // review-fix F6: respawn routes through the backend factory (ADR-0002).
    let backend = backend::multiplexer_backend(cfg)?;
    respawn_with_backend(&*backend, cfg, project, name, kind, force)
}

/// Testable core of `cmd_internal_respawn` (bd-ude8): the mux backend is
/// injected so the omp same-pane path and the argv re-injection are unit-
/// observable; the factory stays at the wrapper.
fn respawn_with_backend(
    backend: &dyn backend::MultiplexerBackend,
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
    force: bool,
) -> Result<()> {
    let ws_id = find_workspace_dyn(backend, project)?;
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
        && let Some(a) = agents
            .iter()
            .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
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
    // bd-ude8: omp must restart in the SAME pane — a fresh split is
    // unnecessary and `pane.close` on the old pane (PTY stdin EOF) kills
    // the respawned omp before it boots. Other kinds keep split+close.
    let same_pane = kind == "omp";
    let new_pane = if same_pane {
        old_pane.clone().ok_or_else(|| {
            anyhow!("omp respawn for {name} in {project}: no live pane to restart in")
        })?
    } else {
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
        let pane = backend.pane_split(&anchor, "down", Some(&utf8_path(&dir, "workspace dir")?))?;
        // best-effort close of the dead pane via the backend trait (herdr
        // implements it with the same `pane.close` RPC).
        if let Some(old) = &old_pane {
            let _ = backend.pane_close(old);
        }
        pane.pane_id
    };
    // bd-ude8: re-inject the profile's model/effort/role launch args on
    // respawn — the old `&[]` argv made the respawned agent fall back to
    // harness defaults (and omp-in-herdr got no flags at all).
    let args = respawn_launch_args(cfg, project, name, kind);
    let fresh = start_agent_with_retry_dyn(backend, name, kind, &new_pane, &args)?;
    // pane-identity bridge bookkeeping (best-effort — never fail the respawn
    // on mail trouble): a same-pane respawn keeps its identity (the pane
    // never died); the split path clears the dead pane's stale identity and
    // pins the fresh pane to the same name (the agent keeps its identity)
    if !same_pane
        && let Ok(mail) = backend::mail_backend(cfg)
        && let Some(key) = dir.to_str()
    {
        if let Some(old) = &old_pane
            && let Err(e) = backend::run_mail(mail.clear_pane_identity(key, old))
        {
            eprintln!("flywheel: mail clear_pane_identity failed for {old}: {e}");
        }
        if let Err(e) =
            backend::run_mail(mail.write_pane_identity(key, &fresh.pane_id, &agent_mail_name(name)))
        {
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

/// bd-ude8: respawn argv = the agent's registry-pinned profile launch args
/// (bd-eign.9 / bd-l2ky parity — the SAME args as the original spawn, never
/// an empty list for a profiled agent). No registry entry, or an entry
/// without a profile (legacy) → empty argv (plain swarm agents keep harness
/// defaults — unchanged). Never fails the respawn on catalog trouble.
fn respawn_launch_args(cfg: &Config, project: &str, name: &str, kind: &str) -> Vec<String> {
    let Ok(key) = project_dir_str(cfg, project) else {
        return Vec::new();
    };
    let profile_name = match state::load_spawn_registry(&key)
        .into_iter()
        .find(|e| e.name == name)
        .and_then(|e| e.profile)
    {
        Some(p) => p,
        None => return Vec::new(),
    };
    let Ok(catalog) = profile::ProfileCatalog::load() else {
        return Vec::new();
    };
    catalog
        .get(&profile_name)
        .map(|p| profile_launch_args(kind, p))
        .unwrap_or_default()
}

/// internal leaf (ADR-0010 Spec E, P10.1): run one prompt headlessly through
/// a harness and log it. The leaf stays DUMB — the runner preprends the role
/// preamble; here: resolve profile → harness, run `harness -p --mode=json`,
/// capture the JSON stream, write `runs/<run_id>.jsonl`, print the outcome.
///
/// Output (json mode): `{ ok, run_id, response, status, tokens_in,
/// tokens_out, model, duration_ms, error? }` — the parent runner parses this
/// one line as the step value.
/// bd-42fs: is a headless outcome rate-limited (HTTP 429 in the harness
/// error stream or stderr)? Pure — the retry-on-429 trigger.
fn is_rate_limited(outcome: &headless::HarnessOutcome) -> bool {
    outcome
        .error
        .as_deref()
        .map(|e| e.contains("429") || e.to_lowercase().contains("rate limit"))
        .unwrap_or(false)
}

fn cmd_internal_run_headless(
    profile_ref: &str,
    prompt: &str,
    model_override: Option<&str>,
    harness_override: Option<&str>,
    workflow: Option<&str>,
    cron: Option<&str>,
    json: bool,
) -> Result<()> {
    use sha1::Digest;

    /// One harness invocation attempt (bd-42fs retry-on-429 support).
    struct RunAttempt {
        model_id: String,
        model: String,
        outcome: headless::HarnessOutcome,
        rate_limited: bool,
    }

    let profile_name = headless::strip_profile_ref(profile_ref);
    let catalog = profile::ProfileCatalog::load()?;
    let profile = catalog
        .get(profile_name)
        .ok_or_else(|| anyhow!("unknown profile @{profile_name} (flywheel profile list)"))?;
    if profile.harness.is_empty() && harness_override.is_none() {
        bail!("profile @{profile_name} has no harness — cannot run headless");
    }
    // bd-42fs step 5: a per-step harness override (spec.harness) wins over
    // the profile's default harness — for THIS step only.
    let harness = harness_override
        .filter(|h| !h.is_empty())
        .unwrap_or(profile.harness.as_str());
    if !matches!(harness, "pi" | "omp" | "opencode" | "grok") {
        bail!("unsupported harness for headless run: {harness} (pi | omp | opencode | grok)");
    }
    // Model resolution (P10.2): the effective id (flag override or the
    // profile's), then the registry's per-harness injection form. Unknown
    // ids warn + pass through — a run never blocks on the registry.
    let model_id = model_override
        .or_else(|| (!profile.model.is_empty()).then_some(profile.model.as_str()))
        .unwrap_or("")
        .to_string();
    let registry = models::ModelRegistry::load()?;
    let model = if model_id.is_empty() {
        String::new()
    } else {
        if registry.get(&model_id).is_none() {
            eprintln!(
                "flywheel: model {model_id:?} not in registry (flywheel model add) — passing through"
            );
        }
        registry.resolve(&model_id, harness)
    };

    // bd-gsvn.2: the omp role models (--smol/--slow/--plan) resolve through
    // the registry the same way as `model` (provider/id form for omp); an
    // unknown role id passes through verbatim, never blocking the run.
    let resolve_role = |id: &str| -> String {
        if id.is_empty() {
            return String::new();
        }
        if registry.get(id).is_none() {
            eprintln!(
                "flywheel: role model {id:?} not in registry (flywheel model add) — passing through"
            );
        }
        registry.resolve(id, harness)
    };
    let role_smol = resolve_role(&profile.smol);
    let role_slow = resolve_role(&profile.slow);
    let role_plan = resolve_role(&profile.plan);

    // bd-42fs retry-on-429: a rate-limited run (HTTP 429 in the harness
    // error stream or stderr) retries ONCE with the model's registry
    // `fallback` (oracle glm-5.2 -> grok, on the grok harness). Thin
    // dynamic-routing slice; the transition is logged via the run entry's
    // `model` (the actual used id).
    let run_once =
        |harness: &str, model_id: &str, model: &str, effort: &str| -> Result<RunAttempt> {
            let argv = headless::build_argv_roles(
                harness,
                (!model.is_empty()).then_some(model),
                (!effort.is_empty()).then_some(effort),
                &role_smol,
                &role_slow,
                &role_plan,
                prompt,
            )
            .map_err(|e| anyhow!("headless run for @{profile_name}: {e}"))?;
            let started = std::time::Instant::now();
            let output = Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .with_context(|| format!("spawn {} for headless run", argv[0]))?;
            let duration_ms = started.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            let mut outcome = headless::parse_stream(&stdout, harness);
            outcome.exit_code = output.status.code().unwrap_or(-1);
            outcome.duration_ms = duration_ms;
            if outcome.response.is_empty() && !stderr.trim().is_empty() {
                outcome.error = Some(stderr.trim().to_string());
            }
            if output.status.code() != Some(0) && outcome.error.is_none() {
                outcome.error = Some(format!("harness exited {}", output.status));
            }
            if outcome.error.is_some() {
                outcome.status = "fail".into();
            }
            let rate_limited = is_rate_limited(&outcome);
            Ok(RunAttempt {
                model_id: model_id.to_string(),
                model: model.to_string(),
                outcome,
                rate_limited,
            })
        };

    let mut attempt = run_once(harness, &model_id, &model, &profile.effort)?;
    let mut retried_fallback = false;
    if attempt.rate_limited && !model_id.is_empty() {
        // fallback = the registry's `fallback` for the primary id; absent ->
        // no retry. A fallback whose entry provider is `grok` runs on the
        // GROK HARNESS (the decided oracle fallback: glm-5.2 -> grok, the
        // premium lane) — the model flag is the bare id there.
        let fallback_entry = registry.get(&model_id).and_then(|e| e.fallback.clone());
        if let Some(fallback_id) = fallback_entry {
            eprintln!(
                "flywheel: @{profile_name} rate-limited on {model_id} (429) — retrying with fallback {fallback_id}"
            );
            let fb_entry = registry.get(&fallback_id);
            let fb_harness = if fb_entry.map(|e| e.provider.as_str()) == Some("grok") {
                "grok"
            } else {
                harness
            };
            let fb_model = registry.resolve(&fallback_id, fb_harness);
            match run_once(fb_harness, &fallback_id, &fb_model, &profile.effort) {
                Ok(fb) => {
                    attempt = fb;
                    retried_fallback = true;
                }
                Err(e) => eprintln!("flywheel: fallback run {fallback_id} failed: {e:#}"),
            }
        }
    }
    let RunAttempt {
        model_id: used_model_id,
        model: used_model,
        outcome,
        ..
    } = attempt;

    let mut hasher = sha1::Sha1::new();
    sha1::Digest::update(&mut hasher, prompt.as_bytes());
    let prompt_hash = hex::encode(sha1::Digest::finalize(hasher));

    let entry = run_log::RunLogEntry {
        run_id: run_log::new_run_id(),
        ts: state::now_iso(),
        workflow: workflow.unwrap_or("").to_string(),
        cron: cron.map(str::to_string),
        profile: profile_name.to_string(),
        harness: harness.to_string(),
        model: used_model,
        mode: "headless".into(),
        prompt_hash,
        response: outcome.response.clone(),
        status: outcome.status.clone(),
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        duration_ms: outcome.duration_ms,
        exit_code: outcome.exit_code,
        error: outcome.error.clone(),
    };
    run_log::write_run(&entry)?;

    if json {
        let mut v = json!({
            "ok": outcome.error.is_none(),
            "run_id": entry.run_id,
            "response": entry.response,
            "status": entry.status,
            "tokens_in": entry.tokens_in,
            "tokens_out": entry.tokens_out,
            "model": entry.model,
            "model_id": used_model_id,
            "retried_fallback": retried_fallback,
            "duration_ms": entry.duration_ms,
        });
        if let Some(e) = &entry.error {
            v["error"] = json!(e);
        }
        print_json(&v);
    } else {
        let status = if outcome.error.is_some() {
            "FAIL"
        } else {
            "ok"
        };
        println!(
            "headless run {} [{status}] {} tok={}+{} {}ms profile={}{}",
            entry.run_id,
            entry.harness,
            entry.tokens_in,
            entry.tokens_out,
            entry.duration_ms,
            entry.profile,
            if retried_fallback {
                format!(" (fallback {used_model_id})")
            } else {
                String::new()
            },
        );
        if let Some(e) = &entry.error {
            eprintln!("headless run error: {e}");
        }
    }
    Ok(())
}

/// The headless ActionRunner dispatch (ADR-0010 Spec A headless row, P10.1):
/// `dispatch_job` resolves the step profile, prepends the behavior preamble
/// (Spec C), and shells out to the `__run-headless` leaf — subprocess-reuse,
/// never an in-process herdr/toron client (Spec A). The leaf's JSON (one
/// line) flows into `{{steps.<name>}}`; its run log carries response + usage.
///
/// Non-dispatch actions stay recorded, never dispatched (P10.1 scope).
///
/// **spawn carve-out:** `Action::Spawn` never reaches this runner — the
/// engine handles it (`run_spawn`: durable promise + SpawnSink → kind-43001,
/// ADR-0005 Spec 4a), and headless MUST NOT use the promise↔job binding
/// (ADR-0010 Spec E). Headless spawn routing + the `await:false` detached
/// branch land with the pane runner (P10.3, bd-ghcj.1). P10.1 headless is
/// dispatch_job-driven — synchronous by engine design.
pub(crate) fn headless_dispatch(
    catalog: &profile::ProfileCatalog,
    wf_name: &str,
    inputs: &Value,
    cron: Option<&str>,
    action: &toron_workflow::schema::Action,
) -> Result<Value> {
    let toron_workflow::schema::Action::DispatchJob { spec, .. } = action else {
        // bd-omwg: a Spawn reaching the runner is an engine-routing bug —
        // fail closed, never record it as done (ADR-0010 Spec E).
        if matches!(action, toron_workflow::schema::Action::Spawn { .. }) {
            bail!(
                "headless runner: Action::Spawn must be handled by the engine (run_spawn) — routing bug"
            );
        }
        // Recorded, not dispatched — the P10.1 backward-compat surface for
        // non-headless actions (byte-identical to the no-op runner).
        return Ok(json!({ "action": action.type_name() }));
    };
    let profile_ref = headless::dispatch_spec_profile(spec)
        .ok_or_else(|| anyhow!("headless dispatch: dispatch_job spec has no profile"))?;
    let profile_name = headless::strip_profile_ref(profile_ref);
    let profile = catalog.get(profile_name).ok_or_else(|| {
        anyhow!("headless dispatch: unknown profile @{profile_name} (flywheel profile list)")
    })?;
    // Role conditioning (Spec C): the authored preamble finally reaches the
    // agent — prepended to the stage prompt (or the goal fallback). The
    // inline `spec.task` (bd-m7ac) wins over the stage: a dispatch with an
    // explicit mission carries it verbatim.
    let mut base = match headless::dispatch_spec_task(spec) {
        Some(task) if !task.trim().is_empty() => task.to_string(),
        _ => match headless::dispatch_spec_stage(spec) {
            Some(s) => stage_prompt(s)?,
            None => headless::base_prompt_for(wf_name, inputs),
        },
    };
    // Workflow-context extras (bd-oj9o): `spec.plan` / `spec.project` carry
    // the run's inputs into the dispatch prompt — the beads plan reaches the
    // planner, the onboard/triage-deep project reaches the crew. Unresolvable
    // refs stay literal; nothing is dropped.
    for key in ["plan", "project"] {
        if let Some(extra) = headless::spec_extra(spec, key, inputs) {
            base.push_str("\n\n");
            base.push_str(key);
            base.push_str(": ");
            base.push_str(&extra);
        }
    }
    let prompt = headless::conditioned_prompt(&profile.behavior, &base);

    // Subprocess-reuse (Spec A): the leaf owns the harness call + run log.
    // `--json` keeps the step output machine-readable (the parent parses
    // stdout as the step value; the human summary would land in error).
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__run-headless")
        .arg(profile_name)
        .arg(&prompt)
        .arg("--workflow")
        .arg(wf_name)
        .arg("--json");
    // bd-42fs step 5: the per-step harness override (spec.harness) rides to
    // the leaf — this step runs on that harness, not the profile default.
    if let Some(h) = headless::dispatch_spec_harness(spec)
        && !h.trim().is_empty()
    {
        cmd.arg("--harness").arg(h);
    }
    if let Some(cron) = cron {
        cmd.arg("--cron").arg(cron);
    }
    if !profile.model.is_empty() {
        cmd.arg("--model").arg(&profile.model);
    }
    let output = cmd
        .output()
        .with_context(|| format!("__run-headless for @{profile_name}"))?;
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let trimmed = raw.trim();
    // The leaf prints ONE json line on success / on captured failure. Any
    // other output (spawn failure, panic) becomes a per-step error value.
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    Ok(json!({ "ok": false, "error": if err.trim().is_empty() { raw } else { err } }))
}

/// bd-omwg: headless fail-closed gate (ADR-0010 Spec E). A workflow carrying
/// a `spawn` step cannot run headless: there is no pane to launch and no
/// honest kind-43001/43004 — faking them is forbidden. Refuse BEFORE the
/// engine invoke (the engine's `run_spawn` would otherwise return
/// `ok:true` with no pane — the exact lie this bead kills). Walks nested
/// steps (retry/branch/foreach/parallel), because a nested spawn is the
/// same lie. Returns the offending step names.
fn workflow_spawn_steps(wf: &toron_workflow::Workflow) -> Vec<String> {
    fn walk(action: &toron_workflow::schema::Action, name: Option<&str>, out: &mut Vec<String>) {
        match action {
            toron_workflow::schema::Action::Spawn { .. } => {
                let n = name.unwrap_or("<unnamed>").to_string();
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            toron_workflow::schema::Action::Retry { step, .. } => walk(&step.action, name, out),
            toron_workflow::schema::Action::Branch {
                then, else_steps, ..
            } => {
                for s in then.iter().chain(else_steps.iter()) {
                    walk(&s.action, s.name.as_deref(), out);
                }
            }
            toron_workflow::schema::Action::Foreach { each, .. } => walk(&each.action, name, out),
            toron_workflow::schema::Action::Parallel { each, .. } => walk(&each.action, name, out),
            _ => {}
        }
    }
    let mut out = vec![];
    for step in &wf.steps {
        walk(&step.action, step.name.as_deref(), &mut out);
    }
    out
}

/// bd-omwg: the gate itself — error names every spawn step and points at
/// pane mode.
pub(crate) fn headless_spawn_gate(wf: &toron_workflow::Workflow) -> Result<()> {
    let steps = workflow_spawn_steps(wf);
    if steps.is_empty() {
        return Ok(());
    }
    bail!(
        "headless cannot run spawn step(s) {:?}: a spawn launches a real pane — use --mode pane; faking kind-43001/43004 is forbidden (ADR-0010 Spec E)",
        steps
    );
}

#[cfg(test)]
mod spawn_drain_gate_tests {
    use super::*;

    fn parse(yaml: &str) -> toron_workflow::Workflow {
        toron_workflow::parse_workflow(yaml).expect("parse workflow")
    }

    /// bd-omwg: a flat spawn step is a headless violation — the gate must
    /// refuse it (ADR-0010 Spec E fail-closed, never fake kind-43001/43004).
    #[test]
    fn headless_spawn_gate_rejects_flat_spawn_step() {
        let wf = parse(
            r##"
name: gate-flat
trigger:
  type: webhook
steps:
  - name: job
    action: spawn
    profile: "@housekeeper"
    await: false
"##,
        );
        let err = headless_spawn_gate(&wf).unwrap_err();
        assert!(err.to_string().contains("job"), "{err}");
        assert!(
            err.to_string().contains("pane"),
            "points at pane mode: {err}"
        );
    }

    /// bd-omwg: a spawn nested inside retry/branch/foreach/parallel is the
    /// same lie — the gate walks nested steps, not just the top level.
    #[test]
    fn headless_spawn_gate_walks_nested_steps() {
        let wf = parse(
            r##"
name: gate-nested
trigger:
  type: webhook
steps:
  - name: outer
    action: branch
    if: "{{inputs.x}}"
    then:
      - name: inner-job
        action: spawn
        profile: "@coder"
        await: true
"##,
        );
        let err = headless_spawn_gate(&wf).unwrap_err();
        assert!(err.to_string().contains("inner-job"), "{err}");
    }

    /// bd-omwg: a spawn-free workflow passes the gate untouched.
    #[test]
    fn headless_spawn_gate_passes_spawn_free_workflow() {
        let wf = parse(
            r##"
name: gate-clean
trigger:
  type: webhook
steps:
  - name: hello
    action: send_dm
    to: operator
    body: "hi"
"##,
        );
        headless_spawn_gate(&wf).expect("no spawn -> pass");
    }
}

/// internal leaf: scan orchestrator inbox for `[xproj]` swarm requests.
/// Soft-fail: missing mail → empty list. Output: `{ "items": [ XprojInboxItem ] }`.
/// internal leaf: mail digest for ONE agent (loop-side wake signal).
///
/// The durable loop calls this per idle agent before dispatch; the digest is
/// prepended to the next prompt so the agent wakes already knowing what peers
/// asked it — without carrying a standing peer-poll template in its context
/// (token-efficient: no per-agent self-poll loop; the orchestrator polls).
///
/// S8.1: the digest RESOLVES the wake identity (spawn-registry /
/// agent-names pin, else the canonical name) and registers it idempotently
/// BEFORE the fetch — the wake prompt's "fetch as X" then names an identity
/// that exists. `mail_identity` rides alongside `agent` (the herdr-name map
/// key the loop uses).
///
/// S8.2: `urgent` counts only wake-filtered rows (read or already-executed
/// assigns are excluded; duplicate assigns collapse).
///
/// Output: `{ "agent": <herdr name>, "mail_identity": <registered identity>,
/// "recent": N, "urgent": N, "last_from": "...", "last_subject": "...",
/// "thread_id": "..." }`.
/// Soft-fail: mail down / unknown agent → zeroed digest, never an error.
fn cmd_internal_mail_digest(cfg: &Config, project: &str, agent: &str) -> Result<()> {
    // `agent` is the caller's key (herdr name from the poll snapshot) —
    // echo it verbatim so the loop can key its digest map by the same name
    // it will look up in plan_actions.
    let key_name = agent.trim().to_string();
    // S8.1: resolve the identity BEFORE the fetch. The registry pin (spawn
    // pinned the identity at spawn) or the project-local agent-names mirror
    // wins; otherwise the canonical name itself — which is what flywheel
    // registers at spawn and what this backstop registers below.
    let identity = {
        let reg = state::load_spawn_registry(project);
        let names = state::load_agent_names(&project_dir(cfg, project));
        wake_identity_pin(&reg, &names, &key_name)
    };
    let zero = json!({
        "agent": key_name, "mail_identity": identity, "unread": 0, "urgent": 0,
        "last_from": null, "last_subject": null, "thread_id": null,
    });
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(_) => {
            print_json(&zero);
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&zero);
        return Ok(());
    }
    // S8.1 + bd-627z.2: first-contact auto-register backstop for the loop's
    // dispatch path. A spawn that ran while the daemon was down left the
    // agent without an identity; the first digest poll (the loop's first
    // contact with the agent's mailbox) registers the RESOLVED identity so
    // inbox/send never 404 "Agent not found". Idempotent upsert;
    // best-effort — a failure just zeroes the digest and the loop retries
    // next poll.
    let auto_profile = backend::AgentProfile {
        name: identity.clone(),
        program: Some("opencode".into()),
        model: Some("opencode".into()),
        persona: None,
        task_description: Some("flywheel mail CLI actor".into()),
    };
    if let Err(e) = backend::run_mail(mail.register_agent(&key, &auto_profile)) {
        eprintln!("flywheel: mail digest auto-register of {identity} failed (zeroing digest): {e}");
    }
    // toron's projection has NO read-state: `unread_only` is a documented
    // no-op ("fresh on first read"). Fetch the RECENT inbox (limit 10) with
    // bodies (the S8.2 dedup keys on body) and report `recent` + the
    // wake-filtered `urgent` (importance high|urgent, assigns filtered).
    let v = match backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key,
        agent: identity.clone(),
        unread_only: false,
        urgent_only: false,
        include_bodies: true,
        limit: Some(10),
        since_ts: None,
    })) {
        Ok(v) => v,
        Err(_) => {
            print_json(&zero);
            return Ok(());
        }
    };
    let v_json = serde_json::to_value(&v).unwrap_or_default();
    let arr = v_json.as_array().cloned().unwrap_or_default();
    let recent = arr.len();
    let urgent_rows = wake_urgent(&arr);
    let urgent = urgent_rows.len();
    let last = arr.first();
    // wake key: the most recent URGENT message that survives the wake
    // filter (the loop wakes once per id)
    let last_urgent = urgent_rows.first().copied().or(last);
    // the wire row names the sender `sender` (toron MCP) or `from`
    // (agent-mail MCP) — read both (MailItem aliases them, but the raw
    // serde_json::Value here is the untyped projection).
    let last_from = last.and_then(|m| {
        m.get("sender")
            .or_else(|| m.get("from"))
            .and_then(|f| f.as_str())
    });
    print_json(&json!({
        "agent": key_name,
        "mail_identity": identity,
        "recent": recent,
        "urgent": urgent,
        "last_id": last_urgent.and_then(|m| {
            m.get("id").or_else(|| m.get("message_id")).and_then(|i| i.as_str())
        }),
        "last_from": last_from,
        "last_subject": last.and_then(|m| m.get("subject").and_then(|s| s.as_str())),
        "thread_id": last.and_then(|m| m.get("thread_id").and_then(|t| t.as_str())),
    }));
    Ok(())
}

/// bd-a7j: newest (message id, sender) in a thread from a sender —
/// recency-ranked, so `verify` = "did the thread move after dispatch?".
///
/// Read via the INBOX PROJECTION (fetch_inbox), not FTS search: hyphenated
/// thread ids are not reliably phrase-searchable in FTS5 (index-coverage
/// gap, cf bd-debate-reply-routing-99ad), so a reply can sit in the
/// orchestrator inbox while `search_messages(thread_id=...)` returns
/// nothing — the check falsely reports "no reply". Inbox rows carry the
/// sender NAME (toron resolves registered npubs) or the raw npub hex —
/// whois resolves the name so both match.
///
/// governed-by: ADR-0001 §9.1 (bd-debate-reply-routing-99ad)
fn thread_head(
    mail: &dyn backend::MailSurface,
    key: &str,
    thread: &str,
    sender: &str,
) -> Result<Option<(String, String)>> {
    let sender_npub = backend::run_mail(mail.whois(key, sender))
        .map(|w| w.npub)
        .unwrap_or_default();
    // limit: None — the inbox projects oldest-first (G7 rumor time), so a
    // bounded `take` would keep the OLDEST rows and could drop the reply.
    let items = backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key.to_string(),
        agent: mail.agent_name().to_string(),
        unread_only: false,
        include_bodies: false,
        limit: None,
        urgent_only: false,
        since_ts: None,
    }))?;
    Ok(newest_in_thread(&items, thread, sender, &sender_npub)
        .map(|m| (m.id.to_string(), m.sender.clone())))
}

/// bd-a7j leaf: newest message id + author in `thread` BY `name` (or nulls).
/// Soft — any mail failure prints nulls ("no reply yet"), mirroring the
/// digest leaf's resilience: verification failures degrade, never abort.
fn cmd_internal_check_reply(cfg: &Config, project: &str, name: &str, thread: &str) -> Result<()> {
    let zero = json!({ "newest_id": null, "author": null });
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(_) => {
            print_json(&zero);
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&zero);
        return Ok(());
    }
    match thread_head(&*mail, &key, thread, &resolve_mail_target(name)) {
        Ok(Some((id, author))) => print_json(&json!({ "newest_id": id, "author": author })),
        _ => print_json(&zero),
    }
    Ok(())
}

/// bd-a7j leaf: log `reply_missing` (checkpointed) and dispatch the CONCRETE
/// reminder by recursing into the `__send` path (same landing semantics,
/// last-sent dedup, confirm handling). One leaf = one checkpointed unit, so
/// crash-resume can neither double-log nor skip the reminder.
fn cmd_internal_remind_reply(
    project: &str,
    name: &str,
    thread: &str,
    reminder_count: u32,
    invocation: Option<&str>,
    iteration: Option<u64>,
) -> Result<()> {
    log(
        project,
        EventKind::ReplyMissing,
        json!({
            "agent": name,
            "thread": thread,
            "reminder": reminder_count + 1,
        }),
    );
    // concrete reminder (the MAIL WAKE lesson: vague prompts get
    // acknowledged in chat, not acted on — name the thread and the command).
    let reminder = format!(
        "REMINDER: your reply is still missing in thread {thread} — send it now via: flywheel mail reply {project} --message-id {thread} --body [your reply] (do not just acknowledge; the orchestrator verifies the thread moved)"
    );
    // recurse into the __send path so the reminder gets identical landing
    // semantics + resume-dedup as any other dispatch.
    let exe = std::env::current_exe()?.canonicalize()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__send").arg(project).arg(name).arg(&reminder);
    cmd.arg("--confirm");
    if let Some(inv) = invocation {
        cmd.arg("--invocation").arg(inv);
        if let Some(it) = iteration {
            cmd.arg("--iteration").arg(it.to_string());
        }
    }
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "__remind-reply: __send failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow!("__remind-reply: bad __send JSON: {e}: {raw}"))?;
    print_json(&v);
    Ok(())
}

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
            false, // skip post-spawn visibility verify (background xproj spawn)
            false, // retire orphan workspaces (xproj spawns are additive)
            false, // not peer-aware by default
            false, // no_peer_aware (xproj spawns stay non-collab)
            5,
            30,
            3,
            30,
            None,
            1,
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

/// internal leaf: mail the planner that a ready bead is NOT dispatchable
/// because its `## VERIFY` fence is missing/multi-line/empty (ADR-0019,
/// bd-w1-verify-fence). Refuse-to-dispatch is the hard guarantee (the loops
/// never queue such a bead); this mail is the advisory. Always soft-fail:
/// exit 0 with `{ok, ...}` so overnight loops never die.
fn cmd_internal_mail_not_ready(
    cfg: &Config,
    project: &str,
    bead_id: &str,
    title: &str,
) -> Result<()> {
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for not-ready alert {bead_id}: {e:#}");
            print_json(&json!({ "ok": false, "bead_id": bead_id, "error": format!("{e:#}") }));
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    let fallback = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "Flywheel".to_string());
    let planner = resolve_planner_for_bead(&key, bead_id, &fallback);
    let out = (|| -> Result<backend::SendReceipt> {
        backend::run_mail(mail.ensure_project(&key)).map_err(|e| anyhow!("{e}"))?;
        send_not_ready_mail(&*mail, &key, bead_id, title, &planner)
    })();
    match out {
        Ok(_) => {
            print_json(&json!({ "ok": true, "bead_id": bead_id, "notified": planner }));
            Ok(())
        }
        Err(e) => {
            eprintln!("flywheel: not-ready alert for {bead_id} failed: {e:#}");
            print_json(&json!({
                "ok": false,
                "bead_id": bead_id,
                "notified": planner,
                "error": format!("{e:#}")
            }));
            Ok(())
        }
    }
}

/// Resolve the planner to notify for a not-ready bead: the bead's creator
/// (`br show --json` → `created_by`), falling back to the configured
/// orchestrator identity when the creator cannot be resolved.
fn resolve_planner_for_bead(project_dir: &str, bead_id: &str, fallback: &str) -> String {
    let out = std::process::Command::new("br")
        .args(["show", bead_id, "--json"])
        .current_dir(project_dir)
        .output();
    if let Ok(out) = out
        && out.status.success()
    {
        if let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) {
            if let Some(created_by) = v
                .as_array()
                .and_then(|a| a.first())
                .and_then(|b| b.get("created_by"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return created_by.to_string();
            }
        }
    }
    fallback.to_string()
}

/// The not-ready mail itself: thread = the bead id, so the planner's reply
/// stays on the bead's thread (ADR-0019 Spec 0). Separated from the leaf
/// command so the "planner mailed" acceptance is testable with a real
/// in-process toron facade.
fn send_not_ready_mail(
    mail: &dyn backend::MailSurface,
    key: &str,
    bead_id: &str,
    title: &str,
    planner: &str,
) -> Result<backend::SendReceipt> {
    let subject =
        format!("[{bead_id}] NOT ready: missing/invalid VERIFY fence — fix before dispatch");
    let body = format!(
        "Bead **{bead_id}** (`{title}`) sits in the ready set but was **NOT dispatched**: \
         its `## VERIFY` fence is missing, empty, or multi-line (ADR-0019).\n\n\
         Required block in the bead description, exact shape:\n\n\
         ```text\n## VERIFY\n```\n\
         <one command, one line>\n```\n```\n\n\
         Name one cheap command the loop can run (e.g. `cargo test -p X name`). \
         Until then the bead stays ready-but-undispatchable — the loop has \
         no fallback verifier for the whole workspace matrix."
    );
    mail_send(
        mail,
        key,
        std::slice::from_ref(&planner.to_string()),
        &subject,
        &body,
        "high",
        true,
        Some(bead_id),
    )
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
    // bd-harness-alt-screen-reads (H4): route through the alt-screen ladder
    // (recent-unwrapped → wait+retry on agent_not_idle → visible → file
    // fallback); still soft on failure — the health mail goes out regardless.
    let mut ops = herdr_shim::BackendAltScreenRead::new(&*backend, name, 80);
    match alt_screen_read_ladder(&mut ops, herdr_shim::RECENT_UNWRAPPED_SOURCE) {
        Ok(v) => {
            tail = v;
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

    // bd-evqs: release reservations whose owning bead is CLOSED. The renewal
    // storm: the loop renewed a lease for bead bd-oj9o (long closed) 1218
    // times — the renew path never looked at the reason. A reservation whose
    // reason is a bead id that is now CLOSED is stale by definition (the work
    // it guarded is done); release it here so the next tick has nothing to
    // renew. Best-effort: br being unavailable (scratch projects, no .beads)
    // keeps today's behavior (renew all, no sweep).
    let mut released_closed: Vec<String> = Vec::new();
    if let Ok(closed) = load_closed_bead_ids(cfg, project) {
        let stale: Vec<String> = v
            .reservations
            .iter()
            .filter(|r| {
                bead_id_from_reason(&r.reason)
                    .map(|id| closed.contains(id))
                    .unwrap_or(false)
            })
            .map(|r| r.path_pattern.clone())
            .collect();
        for path in &stale {
            match backend::run_mail(mail.release_reservations(
                &key,
                &agent,
                std::slice::from_ref(path),
            )) {
                Ok(rel) if rel.released > 0 => {
                    eprintln!("flywheel: released stale reservation {path} (owning bead closed)");
                    released_closed.push(path.clone());
                }
                Ok(_) => {
                    eprintln!("flywheel: release of stale reservation {path} reported 0 released");
                }
                Err(e) => eprintln!(
                    "flywheel: release of stale reservation {path} failed (continuing): {e}"
                ),
            }
        }
    }

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
        "released_closed": released_closed,
        "file_reservations": reservations,
    }));
    Ok(())
}

/// `__release-bead-locks` leaf (bd-h3oj): operator-signed revocation of
/// every live reservation whose reason is the closed bead id. Emits
/// `{"bead_id": ..., "released": n}` for the durable loop's checkpoint.
fn cmd_internal_release_bead_locks(cfg: &Config, project: &str, bead_id: &str) -> Result<()> {
    let mail = backend::mail_backend(cfg)?;
    let key = project_dir_str(cfg, project)?;
    backend::run_mail(mail.ensure_project(&key))?;
    let requestor = orchestrator_mail_name(cfg);
    let v = backend::run_mail(mail.release_by_reason(&key, &requestor, bead_id, true))
        .map_err(|e| anyhow!("mail backend: {e}"))?;
    if v.released > 0 {
        eprintln!(
            "flywheel: released {} reservation(s) for closed bead {bead_id}",
            v.released
        );
    }
    print_json(&json!({
        "bead_id": bead_id,
        "released": v.released,
    }));
    Ok(())
}

/// Parse a reservation reason as a bead id, if the reason names one.
/// Reasons are the bead id verbatim (`bd-oj9o`) or free text; only
/// `bd-<id>`-shaped reasons are treated as bead refs (bd-evqs — the sweep
/// must never guess that an arbitrary reason is a bead).
pub fn bead_id_from_reason(reason: &str) -> Option<&str> {
    let t = reason.trim();
    let id = t.split_whitespace().next().unwrap_or(t);
    // strip trailing punctuation ("bd-12:" or "bd-12," from free-text reasons)
    let id = id.trim_end_matches([':', ',', ';', ')']);
    if id.starts_with("bd-") && id.len() > 3 {
        Some(id)
    } else {
        None
    }
}

/// Load the set of CLOSED bead ids in the project (`br list --all --json`,
/// status == "closed"). Soft-fail: a missing .beads / br error yields an
/// empty set (the sweep no-ops, renew-all preserved).
fn load_closed_bead_ids(cfg: &Config, project: &str) -> Result<std::collections::HashSet<String>> {
    let dir = project_dir(cfg, project);
    let out = std::process::Command::new("br")
        .args(["list", "--all", "--json"])
        .current_dir(&dir)
        .output()
        .map_err(|e| anyhow!("br not found: {e}"))?;
    if !out.status.success() {
        bail!(
            "br list --all failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim())?;
    let mut closed = std::collections::HashSet::new();
    if let Some(issues) = v.get("issues").and_then(Value::as_array) {
        for b in issues {
            let id = b.get("id").and_then(Value::as_str).unwrap_or("");
            let status = b.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "closed" && !id.is_empty() {
                closed.insert(id.to_string());
            }
        }
    }
    Ok(closed)
}

// ---- serve (feature: serve) ----

#[cfg(feature = "serve")]
fn cmd_serve(cfg: &Config, port: u16) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve_async(cfg.clone(), port))
}

#[cfg(feature = "serve")]
async fn serve_async(cfg: config::Config, port: u16) -> Result<()> {
    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
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
    println!(
        "flywheel serve on http://127.0.0.1:{port} (/health /api/v1/snapshot /api/v1/send /events /openapi.json)"
    );
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
    fn render_prompt_has_frontmatter_phase_and_body() {
        let out = render_prompt("opencode", "execute", "9.9.9", "2026-08-04T00:00:00Z").unwrap();
        // prompt shape: YAML frontmatter with the phase description
        assert!(out.starts_with("---\n"));
        assert!(out.contains("Execute one ready bead end-to-end"));
        // phase focus section
        assert!(out.contains("# Beads execute"));
        assert!(out.contains("## This command's focus"));
        // version + generated stamps
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
        assert!(!out.contains("{{TOOL_FRONTMATTER}}"));
        assert!(!out.contains("{{PHASE_DESCRIPTION}}"));
    }

    #[test]
    fn render_all_five_phases_exist_per_kind() {
        for kind in ["opencode", "pi", "grok", "omp"] {
            for phase in ["triage", "execute", "review", "commit", "create"] {
                let out = render_prompt(kind, phase, "0.1.0", "ts").unwrap();
                assert!(
                    out.contains(&format!("# Beads {phase}")),
                    "{kind}/{phase} heading"
                );
                assert!(out.starts_with("---"), "{kind}/{phase} frontmatter");
            }
        }
    }

    #[test]
    fn render_all_kinds_carry_their_config_dir() {
        for (kind, cfg) in [
            ("opencode", "~/.config/opencode"),
            ("pi", "~/.pi"),
            ("grok", "~/.grok"),
            ("omp", "~/.omp"),
        ] {
            let out = render_prompt(kind, "execute", "0.1.0", "ts").unwrap();
            assert!(out.contains(cfg), "{kind} should mention {cfg}");
            assert!(out.starts_with("---"), "{kind} frontmatter");
        }
    }

    #[test]
    fn render_rejects_unknown_kind() {
        assert!(render_prompt("claude", "execute", "0.1.0", "ts").is_err());
        assert!(render_prompt("opencode", "bogus", "0.1.0", "ts").is_err());
    }

    /// bd-oj9o #3 + bd-ucd4: the workflow command set is EXACTLY the popular
    /// user-facing workflows (debate, plan, plan-review, brainstorm, onboard,
    /// triage) — the same rule as the builtin set. Behind-the-scenes
    /// workflows (cron, beads, goal) get no command templates.
    #[test]
    fn workflow_command_set_is_the_popular_workflows() {
        assert_eq!(PROMPT_WORKFLOWS.len(), 7, "exactly seven workflow commands");
        let names: Vec<&str> = PROMPT_WORKFLOWS.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(
            names,
            [
                "debate",
                "plan",
                "plan-review",
                "brainstorm",
                "onboard",
                "triage",
                "captain-audit"
            ]
        );
        for (name, target, description) in PROMPT_WORKFLOWS {
            assert!(
                builtin_workflows::builtin_workflows().contains_key(*target),
                "/{name} targets a real built-in workflow ({target})"
            );
            assert!(!description.is_empty(), "/{name} has a description");
        }
        // triage maps to the triage-deep workflow (the command name is the
        // user-facing short form).
        assert_eq!(
            PROMPT_WORKFLOWS
                .iter()
                .find(|(n, _, _)| *n == "triage")
                .map(|(_, t, _)| *t),
            Some("triage-deep")
        );
        for excluded in ["beads", "goal", "cron"] {
            assert!(
                !names.contains(&excluded),
                "/{excluded} must NOT be a workflow command (behind-the-scenes)"
            );
        }
    }

    /// bd-oj9o #3: a rendered workflow command = frontmatter (with the
    /// workflow description) + the focus section + stamps; no beads body.
    #[test]
    fn render_workflow_prompt_has_frontmatter_and_focus() {
        let out =
            render_workflow_prompt("opencode", "debate", "9.9.9", "2026-08-10T00:00:00Z").unwrap();
        assert!(out.starts_with("---\n"), "frontmatter");
        assert!(
            out.contains("Run the debate workflow"),
            "description in frontmatter"
        );
        assert!(out.contains("# Workflow: debate"));
        assert!(out.contains("## This command's focus"));
        assert!(
            out.contains("flywheel workflow run debate"),
            "the command body tells the agent how to run the workflow"
        );
        assert!(out.contains("flywheel 9.9.9"));
        assert!(out.contains("2026-08-10T00:00:00Z"));
        assert!(!out.contains("{{TOOL_FRONTMATTER}}"));
        assert!(
            !out.contains("{{BODY}}"),
            "workflow commands are standalone — no beads body"
        );
    }

    /// bd-oj9o #3: every kind renders every workflow command.
    #[test]
    fn render_all_workflow_commands_per_kind() {
        for kind in ["opencode", "pi", "grok", "omp"] {
            for (name, _, _) in PROMPT_WORKFLOWS {
                let out = render_workflow_prompt(kind, name, "0.1.0", "ts").unwrap();
                assert!(out.starts_with("---"), "{kind}/{name} frontmatter");
                assert!(
                    out.contains(&format!("# Workflow: {name}")),
                    "{kind}/{name} heading"
                );
            }
        }
    }

    /// bd-oj9o #3: unknown kind or command is an actionable error.
    #[test]
    fn render_workflow_rejects_unknown() {
        assert!(render_workflow_prompt("claude", "debate", "0.1.0", "ts").is_err());
        assert!(render_workflow_prompt("opencode", "beads", "0.1.0", "ts").is_err());
    }

    #[test]
    fn render_contains_lifecycle_and_context_contract() {
        let out = render_prompt("pi", "execute", "0.1.0", "ts").unwrap();
        assert!(out.contains("br update bd-<id> --claim"));
        assert!(out.contains("br sync --flush-only"));
        assert!(out.contains("CONTEXT_LIMIT"));
        assert!(out.contains(".flywheel/agent-state/<AGENT_NAME>.json"));
        assert!(out.contains("bv --robot-triage"));
        assert!(out.contains("file_reservation_paths"));
        assert!(out.contains("Never use `bd`"));
    }

    #[test]
    fn prompts_dir_maps_each_kind() {
        // config tests mutate HOME under the env lock and leave it removed —
        // pin HOME for the mapping's lifetime (never read the ambient value).
        let _g = crate::config::lock_env();
        let home = std::env::temp_dir().join(format!("fw-prompts-home-{}", std::process::id()));
        unsafe { std::env::set_var("HOME", &home) };
        for (kind, sub) in [
            ("opencode", ".config/opencode/command"),
            ("pi", ".pi/agent/prompts"),
            ("grok", ".grok/prompts"),
            ("omp", ".omp/agent/prompts"),
        ] {
            let p = prompts_dir(kind).expect(kind);
            assert!(
                p.ends_with(sub),
                "{kind}: {} should end with {sub}",
                p.display()
            );
            assert!(!p.to_string_lossy().is_empty());
        }
        assert!(prompts_dir("claude").is_none());
        unsafe { std::env::remove_var("HOME") };
    }
}

#[cfg(test)]
mod headless_retry_tests {
    use super::*;

    fn outcome_with_error(err: Option<&str>) -> headless::HarnessOutcome {
        headless::HarnessOutcome {
            response: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            status: if err.is_some() {
                "fail".into()
            } else {
                "success".into()
            },
            model: String::new(),
            duration_ms: 0,
            exit_code: if err.is_some() { 1 } else { 0 },
            error: err.map(str::to_string),
        }
    }

    /// bd-42fs: HTTP 429 (in the harness error stream or stderr) is the
    /// retry-on-429 trigger; other errors are not.
    #[test]
    fn rate_limit_predicate_classifies_429() {
        assert!(is_rate_limited(&outcome_with_error(Some(
            "Error: 429 Too Many Requests"
        ))));
        assert!(is_rate_limited(&outcome_with_error(Some(
            "rate limit exceeded"
        ))));
        assert!(is_rate_limited(&outcome_with_error(Some("RATE LIMIT"))));
        assert!(is_rate_limited(&outcome_with_error(Some(
            "{\"error\":{\"code\":429}}"
        ))));
        assert!(!is_rate_limited(&outcome_with_error(Some(
            "model not found"
        ))));
        assert!(!is_rate_limited(&outcome_with_error(Some(
            "401 unauthorized"
        ))));
        assert!(!is_rate_limited(&outcome_with_error(None)));
    }

    /// bd-42fs: the oracle's fallback chain resolves — glm-5.2 (zai) with a
    /// 429 retries on grok-4.5 (the registry fallback), injected for the
    /// grok harness (bare id).
    #[test]
    fn oracle_fallback_resolves_via_registry() {
        let _g = crate::config::lock_env();
        let mut reg = models::ModelRegistry::default();
        reg.models.insert(
            "glm-5.2".into(),
            models::ModelEntry {
                provider: "zai".into(),
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: Some("grok-4.5".into()),
                effort: Some("max".into()),
            },
        );
        reg.models.insert(
            "grok-4.5".into(),
            models::ModelEntry {
                provider: "grok".into(),
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: None,
                effort: Some("max".into()),
            },
        );
        let fb = reg
            .get("glm-5.2")
            .and_then(|e| e.fallback.clone())
            .expect("fallback declared");
        assert_eq!(fb, "grok-4.5");
        assert_eq!(
            reg.resolve(&fb, "grok"),
            "grok-4.5",
            "grok takes the bare id"
        );
        assert_eq!(
            reg.resolve(&fb, "omp"),
            "grok/grok-4.5",
            "omp prefixes the provider"
        );
        // no fallback declared -> no retry (empty).
        let no_fb = reg.get("grok-4.5").and_then(|e| e.fallback.clone());
        assert!(no_fb.is_none());
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

    /// bd-debate-harvest-dsww: inbox-projection selection replaces FTS
    /// search — a hyphenated thread id (`debate-20260806T130548`) must
    /// match EXACTLY on the row's thread_id (no FTS tokenization), the
    /// newest in-thread row from the sender wins, and rows outside the
    /// thread or from other senders never leak in.
    #[test]
    fn newest_in_thread_matches_exact_thread_and_newest() {
        let mk =
            |id: &str, sender: &str, thread: Option<&str>, ts: i64, body: &str| backend::MailItem {
                id: backend::MessageId::from(id),
                sender: sender.to_string(),
                subject: format!("Re: [debate] prop {id}"),
                body_md: body.to_string(),
                importance: "high".to_string(),
                ack_required: true,
                thread_id: thread.map(backend::MessageId::from),
                created_ts_us: ts,
                duplicate_of: None,
            };
        let thread = "debate-20260806T130548";
        let rows = vec![
            mk("o1", "CoralFinch", Some(thread), 1_000, "opening prompt"),
            mk("r1", "TopazOx", Some(thread), 2_000, "counter one"),
            mk("r2", "TopazOx", Some(thread), 3_000, "counter two"),
            mk("x1", "TopazOx", Some("debate-other"), 9_000, "other thread"),
            mk(
                "x2",
                "CoralFinch",
                Some(thread),
                9_500,
                "orchestrator prompt",
            ),
        ];
        // newest TopazOx row in THIS thread wins, other thread/sender excluded
        let hit = newest_in_thread(&rows, thread, "TopazOx", "").expect("reply must be found");
        assert_eq!(hit.id.to_string(), "r2");
        assert_eq!(hit.body_md, "counter two");
        // no reply yet → None (harvest then bails loudly, never ships empty)
        assert!(newest_in_thread(&rows, thread, "NeverReplied", "").is_none());
    }

    /// bd-debate-harvest-dsww / bd-debate-reply-routing-99ad: the sender
    /// matches by NAME (toron resolves registered npubs, agent-mail returns
    /// `from`) AND by raw npub hex (toron's fallback when the sender never
    /// registered) — both inbox projections land in the same match.
    #[test]
    fn newest_in_thread_matches_name_or_npub() {
        let mk = |id: &str, sender: &str, ts: i64| backend::MailItem {
            id: backend::MessageId::from(id),
            sender: sender.to_string(),
            subject: "Re: [debate] prop".to_string(),
            body_md: "arg".to_string(),
            importance: "high".to_string(),
            ack_required: true,
            thread_id: Some(backend::MessageId::from("debate-20260806T130548")),
            created_ts_us: ts,
            duplicate_of: None,
        };
        let npub = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        // name-only rows: the NAME matches
        let rows = vec![mk("n1", "TopazOx", 1_000), mk("n3", "someone-else", 3_000)];
        let by_name = newest_in_thread(&rows, "debate-20260806T130548", "TopazOx", npub).unwrap();
        assert_eq!(by_name.id.to_string(), "n1");
        // raw-hex rows only (unregistered sender): the NPUB matches
        let rows = vec![mk("n2", npub, 2_000), mk("n3", "someone-else", 3_000)];
        let by_npub = newest_in_thread(&rows, "debate-20260806T130548", "TopazOx", npub).unwrap();
        assert_eq!(by_npub.id.to_string(), "n2");
        // npub unknown AND name absent → no match against the hex row
        assert!(newest_in_thread(&rows, "debate-20260806T130548", "TopazOx", "").is_none());
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
        for name in [
            "oracle",
            "planner",
            "reviewer",
            "coder",
            "scout",
            "housekeeper",
        ] {
            assert!(is_builtin_profile(name), "{name} is a built-in");
        }
        assert!(!is_builtin_profile("mybot"));
        assert!(!is_builtin_profile(""));
    }

    #[test]
    fn profile_catalog_lists_seven_builtins() {
        let cat = profile::ProfileCatalog::load_from(None).expect("defaults");
        assert_eq!(cat.names().len(), 7);
        for name in [
            "oracle",
            "planner",
            "reviewer",
            "coder",
            "scout",
            "housekeeper",
        ] {
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
        assert_eq!(r.spec.harness, "omp");
        assert_eq!(r.spec.model, "deepseek-v4-flash");
        assert_eq!(r.spec.backend, "herdr");
        assert_eq!(
            r.signing,
            profile::SigningContext::Operator("npub-op".into())
        );
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
        assert_eq!(names.len(), 7);
    }

    #[test]
    fn snapshot_runs_returns_well_formed_entries() {
        // Runs section is always a JSON array; every entry carries the fields
        // the agent driver contracts on (id/project/kind/owner/state/ts).
        let runs = snapshot_runs();
        for r in &runs {
            assert!(r.get("id").and_then(Value::as_str).is_some(), "id");
            assert!(
                r.get("project").and_then(Value::as_str).is_some(),
                "project"
            );
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

/// bd-bvwf.1 regression: --retire-orphans must catch orphan workspaces from
/// prior spawns (not just the default-label one). An orphan with a DIFFERENT
/// label that still hosts a `flywheel-<base>-*` agent is classified Owned and
/// retired; the loop's poll filters to the active workspace so orphans never
/// enter the dispatch decision.
#[cfg(test)]
mod bd_bvwf1_orphan_workspaces {
    use super::{WorkspaceOrigin, classify_workspace_origin, partition_workspaces};
    use crate::herdr_shim::{AgentInfo, WorkspaceInfo};

    fn ws(id: &str, label: &str) -> WorkspaceInfo {
        serde_json::from_value(serde_json::json!({
            "workspace_id": id,
            "number": 1,
            "label": label,
            "focused": false,
            "pane_count": 1,
            "tab_count": 1,
            "active_tab_id": "t",
            "agent_status": "idle",
        }))
        .expect("ws")
    }

    fn agent(ws_id: &str, name: Option<&str>) -> AgentInfo {
        serde_json::from_value(serde_json::json!({
            "agent": null,
            "agent_status": "idle",
            "cwd": null,
            "display_agent": null,
            "focused": false,
            "foreground_cwd": null,
            "interactive_ready": false,
            "launch_pending": false,
            "name": name,
            "pane_id": format!("{ws_id}:p1"),
            "revision": 1,
            "state_change_seq": 0,
            "state_labels": {},
            "tab_id": "t",
            "terminal_id": "x",
            "terminal_title": null,
            "workspace_id": ws_id,
        }))
        .expect("agent")
    }

    /// An orphan workspace with a NON-default label (e.g. a custom spawn) that
    /// hosts a `flywheel-<base>-*` agent is Owned — retired by --retire-orphans.
    /// (The original bug: only the default-label workspace was closed.)
    #[test]
    fn orphan_hosting_managed_agent_is_owned() {
        let project = "/path/to/toron";
        // default-label workspace (always owned)
        let default_ws = ws("w-default", "flywheel-toron");
        // orphan: different label, but hosts a managed agent
        let orphan_ws = ws("w-orphan", "custom-swarm-label");
        let agents = vec![
            agent("w-orphan", Some("flywheel-toron-oc")),
            agent("w-foreign", Some("omp")),
        ];
        assert_eq!(
            classify_workspace_origin(project, &default_ws, &agents, None),
            Some(WorkspaceOrigin::Owned)
        );
        assert_eq!(
            classify_workspace_origin(project, &orphan_ws, &agents, None),
            Some(WorkspaceOrigin::Owned),
            "orphan hosting a flywheel-<base>-* agent is Owned (retired)"
        );
    }

    /// partition_workspaces groups the default + orphan into `owned` and a
    /// base-labeled foreign session into `foreign`; unrelated workspaces drop.
    #[test]
    fn partition_owns_default_and_orphan_foreign_is_base_label() {
        let project = "/path/to/toron";
        let ws_list = vec![
            ws("w-default", "flywheel-toron"),
            ws("w-orphan", "custom-swarm-label"),
            ws("w-captain", "toron"),
            ws("w-unrelated", "some-other-project"),
        ];
        let agents = vec![agent("w-orphan", Some("flywheel-toron-oc2"))];
        let split = partition_workspaces(project, ws_list, &agents, None);
        let owned_ids: Vec<&str> = split
            .owned
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect();
        let foreign_ids: Vec<&str> = split
            .foreign
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect();
        assert_eq!(
            owned_ids,
            vec!["w-default", "w-orphan"],
            "default + orphan owned"
        );
        assert_eq!(
            foreign_ids,
            vec!["w-captain"],
            "base-label session is foreign"
        );
        // unrelated workspace is neither (None) — never blocks or closes.
    }
}

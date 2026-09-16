//! `flywheel loop` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Subcommands of `flywheel loop` (bd-fw-stale-loop-processes-gac
/// restructure): `run` (default), `status`, `list`, `prune`.
#[derive(Subcommand)]
pub(crate) enum LoopCmd {
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
    /// Re-drive the recorded Msg journal through tick, assert identical actions (ADR-0025 Spec 3)
    Replay { project: String },
    /// Print (msg, state-delta, actions) triple for a journal seq (ADR-0025 Spec 3)
    Why { project: String, seq: usize },
}

//! `flywheel workflow` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Unified workflow commands (ADR-0005 Spec 5 / P3.7). `lint` is the
/// fail-stop gate every run must pass; `crew <name>` previews the auto-crew
/// a workflow would spawn.
#[derive(Subcommand)]
pub(crate) enum WorkflowCmd {
    /// List catalog + built-in workflows (name, source, trigger, crew).
    /// `--project` layers the project tier (built-ins < user < project),
    /// matching the `run`/`compose` resolution order.
    List {
        /// accepted: list is always JSON (agent `--json` parity)
        #[arg(long)]
        json: bool,
        /// GS-052: hybrid brief — name/family/selection/run/topology (+ compose_token); decide_trigger/full row on bare list; show is prose/graph (no decide_trigger)
        #[arg(long)]
        brief: bool,
        /// project whose `.flywheel/workflows/` shadows the user catalog
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one workflow's definition
    Show {
        /// workflow name (built-in or catalog)
        name: String,
        /// project whose `.flywheel/workflows/` shadows the user catalog
        #[arg(long)]
        project: Option<String>,
    },
    /// Lint ALL built-in + catalog workflows (the fail-stop gate). Exit 0
    /// clean, 1 on any blocking issue, 2 on a parse failure.
    Lint {
        /// lint only this workflow (name), else all
        name: Option<String>,
        /// project whose `.flywheel/workflows/` joins the lint set
        #[arg(long)]
        project: Option<String>,
    },
    /// GS-049 / GS-045 P1-2: evaluate-without-commit — lint + partition each
    /// step into validated candidate outcomes without journal dispatch.
    /// Host path (`workflow run --execute`) stays authoritative for live effects.
    Evaluate {
        /// workflow name
        name: String,
        /// project whose `.flywheel/workflows/` joins the catalog
        #[arg(long)]
        project: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Run a workflow by name through the unified engine (lint gate first).
    /// Ordinary builtins (e.g. captain-audit): `workflow run <name> --execute`.
    /// Compose tokens (`wave-swarm`, `repair-until-verified`,
    /// `beads-lifecycle`): prefer
    /// `workflow compose … --parts <token> --run`; `workflow run <token>
    /// --execute` is the legacy alias. Do NOT pass ordinary builtins as bare
    /// `--parts`. Interactive TTY briefing when `--execute` (bd-nmzk): same
    /// skip matrix as `steer` — shared prompt module (`source = workflow`).
    Run {
        /// workflow name (built-in or catalog); compose tokens accepted as legacy alias
        name: String,
        /// JSON inputs for the invoke path ({{inputs.x}})
        #[arg(long)]
        inputs: Option<String>,
        /// crew strategy: auto | <N total panes> | role:n,role:n | <squad> (ADR-0011 D2)
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
        /// skip the interactive captain briefing (bd-nmzk; implied by --json
        /// and an explicit --crew — shared surface with `steer`)
        #[arg(long)]
        yes: bool,
        /// ADR-0019 Spec 3.3 (operator-only, logged): allow `workflow run
        /// --execute` in a workspace that carries an extra linked worktree.
        /// Requires the operator key.
        #[arg(long)]
        allow_worktree: bool,
        /// Per-role model overlay (same pin as compose `--role-model`).
        /// Bound from TTY captain briefing; agents pass the flag after remix.
        #[arg(long = "role-model", value_name = "ROLE=PROVIDER/MODEL")]
        role_model: Vec<String>,
        /// Per-role harness overlay (`coder=pi`). Bound from TTY captain
        /// briefing; aliases `oc`/`gk` normalize. Unknown kind fails loud.
        #[arg(long = "role-harness", value_name = "ROLE=KIND")]
        role_harness: Vec<String>,
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
        /// parts: comma-separated `stage:<stage>`, `role:<role>:<stage>`,
        /// `phase:<phase>:<stage>`, compact `<role>:<stage>` / `<phase>:<stage>`,
        /// `workflow:<name>`, bare stage, or compose token. Control forms:
        /// `each:<param>:<part>`, `parallel:<param>:<part>`, `when:<guard>:<part>`,
        /// `retry(n):<part>`. Tokens: wave-swarm / repair-until-verified /
        /// beads-lifecycle. Preferred door for tokens: `compose … --parts <token> --run`.
        #[arg(long, value_delimiter = ',')]
        parts: Vec<String>,
        /// planner arm: an agent authors the workflow from the target
        #[arg(long)]
        agent: bool,
        /// T2 (§3.6): author a FULL Workflow from YAML text instead of
        /// assembling from parts — `-` reads stdin. Exclusive with
        /// --agent/--parts/--base.
        #[arg(long)]
        from_yaml: Option<String>,
        /// §3.6 safety envelope: structural dry-run — the resolved graph
        /// with fanout counts + phase tags. NEVER dispatches.
        #[arg(long)]
        dry_run: bool,
        /// run the composed workflow (lint gate first, pane mode)
        #[arg(long)]
        run: bool,
        /// W6 (§3.7 T3): after a GREEN --run, graduate the composed workflow
        /// into the project catalog (.flywheel/workflows/<name>.yaml) with
        /// provenance. Refuses without a green run.
        #[arg(long)]
        keep: Option<String>,
        /// pane mode: the project whose workspace hosts the crew
        #[arg(long)]
        project: Option<String>,
        /// bd-wave-swarm-graduation-fix-98io: run inputs as a JSON object
        /// (merged over {"objective": target}) — e.g. the orchestrator pin
        /// the compose run escalates to: --inputs '{"captain_pin":"<pin>"}'
        #[arg(long)]
        inputs: Option<String>,
        /// ADR-0025 W5: stable idempotency key for compose --run (project, bead_id|run_id, profile) — second run with same key returns existing run
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Per-role model routing for compose --run (repeatable).
        /// `coder=opencode-go/muse-spark-1.2-contributor` uses the same
        /// registry pin as `spawn --profile`.
        #[arg(long = "role-model", value_name = "ROLE=PROVIDER/MODEL")]
        role_model: Vec<String>,
        /// Per-role harness overlay (`coder=pi`). Bound from TTY briefing.
        /// Applied before blanket `--harness` (the blanket still wins).
        #[arg(long = "role-harness", value_name = "ROLE=KIND")]
        role_harness: Vec<String>,
        /// bd-enov (F6): per-run harness override for compose --run — every
        /// composed role spawns on KIND (e.g. `opencode`). Removes the need to
        /// mutate `~/.config/flywheel/profiles.toml` for a one-off run.
        /// Validated against the herdr kind registry + `[harness.*]` recipes;
        /// `oc`/`gk` normalize to `opencode`/`grok`.
        #[arg(long, value_name = "KIND")]
        harness: Option<String>,
        /// bd-enov (F7/F8): wall-clock budget for the composed run in seconds.
        /// On expiry the run fails LOUD (never hangs in the foreground); the
        /// durable run stays resumable. Overrides
        /// FLYWHEEL_COMPOSE_MAX_SECONDS (default 3600).
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// GS-039 P0-3: NEVER silent-reap desks. Default OFF — pass
        /// `--retire-orphans` explicitly to close pre-existing workspaces.
        // GS-040 B2: hidden `--no-retire-orphans` is a SEPARATE field linked
        // via overrides_with — NOT dual-#[arg] on the same field (that
        // dropped the positive flag; only --no-retire-orphans survived).
        #[arg(
            long = "retire-orphans",
            action = clap::ArgAction::SetTrue,
            overrides_with = "_no_retire_orphans"
        )]
        retire_orphans: bool,
        /// Hidden SetFalse alias for script compat (GS-040 B2).
        #[arg(
            long = "no-retire-orphans",
            action = clap::ArgAction::SetFalse,
            overrides_with = "retire_orphans",
            hide = true
        )]
        _no_retire_orphans: bool,
        /// crew strategy: auto | <N total panes> | role:n,role:n | <squad> (ADR-0011 D2)
        #[arg(long)]
        crew: Option<String>,
        /// operator pane cap override (ADR-0011 D2). Default crew.max=5.
        #[arg(long = "crew-max")]
        crew_max: Option<u32>,
        /// Deprecated: packed-under-cap no longer needs a confirm (ADR-0011 D2).
        /// Kept so old argv does not hard-bail; does not raise the cap.
        #[arg(long = "allow-crew-expand", default_value_t = false)]
        allow_crew_expand: bool,
        /// pane mode: bind an existing workspace id instead of spawning fresh
        #[arg(long)]
        reuse: Option<String>,
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Seed compiled-in workflow and stage defaults into the user tier
    Seed {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// How much of the user tier may be REPLACED.
        ///
        ///   none   fill gaps only; touch nothing that exists (DEFAULT)
        ///   stale  repair builtin-named copies that `workflow drift` calls stale
        ///   all    restore every builtin-named copy, stale or not
        ///
        /// A name the binary does not ship is never overwritten at any scope, so
        /// an authored workflow is always safe. Every replaced file keeps its
        /// old bytes at `<name>.bak`.
        #[arg(long, value_name = "SCOPE", value_parser = ["none", "stale", "all"], default_value = "none")]
        overwrite: String,
        /// Report the plan and change nothing. Use before --overwrite.
        #[arg(long)]
        dry_run: bool,
        /// Accept a destructive overwrite without prompting. REQUIRED when there
        /// is no TTY: a prompt nobody can answer is not consent.
        #[arg(long)]
        yes: bool,
    },
    /// Report user-tier workflow/stage copies that are missing, stale, or custom
    Drift {
        /// JSON output for agent drivers
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
        /// project whose `.flywheel/workflows/` shadows the user catalog
        #[arg(long)]
        project: Option<String>,
    },
    /// Remove a catalog workflow (deletes ~/.config/flywheel/workflows/<name>.yaml).
    /// Built-in workflows cannot be removed — override via a catalog file of the
    /// same name (catalog shadows built-ins).
    Remove {
        /// workflow name (catalog only)
        name: String,
    },
    /// Promote a workflow up the §3.8 permanence ladder
    /// (project → user → repo). The user write goes through the SAME
    /// writer as compose --keep, so both lint identically.
    Promote {
        /// workflow name (must exist at project or user tier)
        name: String,
        /// target tier: user | repo
        #[arg(long, value_parser = ["user", "repo"])]
        to: String,
        /// repo tier only: >=2 successful run ids as evidence
        #[arg(long, value_delimiter = ',')]
        evidence: Vec<String>,
        /// repo tier only: the review bead id witnessing the promotion
        #[arg(long)]
        review_bead: Option<String>,
    },
    /// Approve or reject a parked SIGNOFF promise (bd-1ub5): sends the
    /// decision mail the parked run's approval drainer resolves.
    Approve {
        /// run id that parked on approval
        run_id: String,
        /// promise id to resolve
        promise_id: String,
        /// decision: approve | reject
        #[arg(long, value_parser = ["approve", "reject"], default_value = "approve")]
        decision: String,
        /// optional reason
        #[arg(long)]
        reason: Option<String>,
        /// the project whose parked run holds the promise (default flywheel)
        #[arg(long, default_value = "flywheel")]
        project: String,
    },
}

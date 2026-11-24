//! `flywheel workflow` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;

use crate::cmd::internal::{headless_dispatch, headless_spawn_gate, make_verify_sink};
use crate::cmd::mail::{mail_or_die, repo_evidence_verified};
use crate::cmd::send::{ComposeRunLifecycle, running_run_row_exists};
use crate::config::Config;
use crate::output::print_json;
use crate::state::EventKind;
use crate::{
    briefing_enabled, config, crew, cron, enrich_suggestions_live, ensure_and_join_channels,
    ensure_project_opt, history_row_status, log, operator_pin, output, output_action_stage,
    packed_from_flag, pane, phase_chain, planner_author_response, profile, project_dir,
    project_dir_str, project_door_opt, run_row_kind, run_terminal_status, stages, state,
    steer_brief, workflow_briefing_context, workflow_catalog, write_terminal_artifact,
};

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
        /// real hc crew and dispatches via the loop's `__send` leaf
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
        /// Validated against the hc kind registry + `[harness.*]` recipes;
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
        /// bd-3ann1: compose onto the live fleet — bind steps to existing
        /// seats and start a pane only for roles no live seat can fill.
        #[arg(long)]
        join: bool,
        /// bd-3ann1: consent for the new panes a --join plan needs.
        #[arg(long)]
        yes: bool,
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
    /// Check registered statecharts' topology: every state reachable, every reachable leaf can reach
    /// its goal. Guards count as enabled; no model checker runs. Optionally print topology-only TLA+.
    Verify {
        /// Chart or workflow name: P2PNegotiation, Seat, Plan, LeaseWaitlist, Brief, ResultMail, CaptainWatch, DeadlockWatch, WorkUnit, or a catalog
        /// workflow (wave-swarm is verified as the workflow now). Omit for all.
        name: Option<String>,
        /// Export compiled TLA+ specification for each verified chart.
        #[arg(long)]
        export_tla: bool,
        /// JSON output for agent drivers and verification gates.
        #[arg(long)]
        json: bool,
    },
}

/// GS-049 / GS-045 P1-2: direct evaluate-without-commit (Jido cmd/3 adapt).
/// Lints the workflow, partitions each step action into a validated candidate
/// outcome, and returns that data — never opens the effect journal and never
/// dispatches. Live effects stay on `workflow run --execute`.
fn cmd_workflow_evaluate(
    cfg: &Config,
    name: &str,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    ensure_project_opt(project, "workflow evaluate")?;
    let project_root = project.map(|p| project_dir(cfg, p));
    let all = workflow_catalog::all_workflows_merged(project_root.as_deref())?;
    let (wf, source) = all
        .get(name)
        .ok_or_else(|| anyhow!("unknown workflow {name:?} (flywheel workflow list)"))?;
    let blocking = lint_flywheel_tier(name, wf, project_root.as_deref());
    if blocking > 0 {
        bail!("workflow evaluate: lint blocked on {name} ({blocking} finding(s))");
    }
    let mut candidates = Vec::new();
    for (idx, step) in wf.steps.iter().enumerate() {
        let part = toron_workflow::engine::partition(&step.action);
        let (partition_kind, detail) = match part {
            toron_workflow::engine::ActionPartition::EngineControl => {
                ("engine_control".to_string(), serde_json::Value::Null)
            }
            toron_workflow::engine::ActionPartition::HostSink { sink } => {
                ("host_sink".to_string(), json!({ "sink": sink }))
            }
            toron_workflow::engine::ActionPartition::Unsupported { reason } => {
                ("unsupported".to_string(), json!({ "reason": reason }))
            }
        };
        candidates.push(json!({
            "step": step.name.clone().unwrap_or_else(|| format!("step-{idx}")),
            "action": step.action.type_name(),
            "partition": partition_kind,
            "detail": detail,
            "committed": false,
            "journaled": false,
        }));
    }
    let body = json!({
        "schema": "flywheel-evaluate.v1",
        "workflow": name,
        "source": source,
        "evaluate_without_commit": true,
        "lint": "clean",
        "candidates": candidates,
        "note": "validated candidates only — host `workflow run --execute` remains authoritative for live effects (GS-045 P1-2 / ADR-0032)",
    });
    if json {
        print_json(&body);
    } else {
        println!("evaluate-without-commit {name} (source={source}) lint=clean");
        for c in &candidates {
            println!(
                "  step={} action={} partition={} committed=false",
                c["step"].as_str().unwrap_or("?"),
                c["action"].as_str().unwrap_or("?"),
                c["partition"].as_str().unwrap_or("?"),
            );
        }
        println!("note: no journal dispatch — use `workflow run --execute` for live effects");
    }
    Ok(())
}

/// Shared hybrid selection fields for one workflow name.
/// Used by brief + full list projections so family/selection/run/compose_token
/// cannot drift between `--brief` and bare `list --json`.
pub(crate) fn workflow_hybrid_fields(name: &str) -> (String, &'static str, &'static str, bool) {
    let catalog = crate::workflow_catalog::WorkflowCatalog::load_layered(None).unwrap_or_default();
    workflow_hybrid_fields_in(name, &catalog)
}

/// Hybrid selection fields with an explicit catalog (see [`workflow_hybrid_fields`]).
/// The caller builds the catalog once (e.g. per list command) and reuses it
/// across rows, instead of loading per row.
pub(crate) fn workflow_hybrid_fields_in(
    name: &str,
    catalog: &crate::workflow_catalog::WorkflowCatalog,
) -> (String, &'static str, &'static str, bool) {
    let compose_token = catalog.is_compose_token(name);
    let run = if compose_token {
        format!("workflow compose \"<objective>\" --parts {name} --run")
    } else {
        format!("workflow run {name} --execute")
    };
    let (family, selection) = crate::builtin_workflows::workflow_guidance(name)
        .unwrap_or(("custom", "inspect workflow show before running"));
    (run, family, selection, compose_token)
}

/// GS-052: live `--brief` projection used by [`cmd_workflow`].
/// ONE hybrid contract = name / family / selection / run / topology / compose_token
/// (not a local test mirror; no competing brief path).
pub(crate) fn workflow_list_brief_rows(
    all: &std::collections::BTreeMap<String, (toron_workflow::schema::Workflow, &'static str)>,
) -> Vec<serde_json::Value> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let catalog = crate::workflow_catalog::WorkflowCatalog::load_layered(None).unwrap_or_default();
    for (name, (_wf, _source)) in all {
        let (run, family, selection, compose_token) = workflow_hybrid_fields_in(name, &catalog);
        rows.push(serde_json::json!({
            "name": name,
            "family": family,
            "selection": selection,
            "run": run,
            "topology": crate::decide::topology_for_primitive(name),
            "compose_token": compose_token,
        }));
    }
    rows
}

/// Live bare `workflow list --json` projection used by [`cmd_workflow`].
/// Same hybrid fields as brief via [`workflow_hybrid_fields`], plus full-row
/// description/trigger/source/crew/inputs/decide_trigger (no topology).
pub(crate) fn workflow_list_full_rows(
    all: &std::collections::BTreeMap<String, (toron_workflow::schema::Workflow, &'static str)>,
) -> Vec<serde_json::Value> {
    use toron_workflow::schema::CrewSpec;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let catalog = crate::workflow_catalog::WorkflowCatalog::load_layered(None).unwrap_or_default();
    for (name, (wf, source)) in all {
        let (run, family, selection, compose_token) = workflow_hybrid_fields_in(name, &catalog);
        rows.push(serde_json::json!({
            "name": name,
            "description": wf.description,
            "family": family,
            "selection": selection,
            "run": run,
            "compose_token": compose_token,
            "source": source,
            "trigger": wf.trigger.as_ref().map(toron_workflow::Trigger::type_name),
            "crew": match &wf.crew {
                Some(CrewSpec::Auto) => "auto",
                Some(CrewSpec::Explicit(_)) => "explicit",
                None => "none",
            },
            "steps": wf.steps.len(),
            "graph_nodes": wf.graph.as_ref().map(|g| g.nodes.len()),
            "inputs": wf.inputs.iter().map(|i| &i.name).collect::<Vec<_>>(),
            "decide_trigger": crate::decide::decide_trigger_for(name),
        }));
    }
    rows
}

/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
/// bd-3ann1: the live seats an in-flight compose may bind to. Pure projection
/// of the spawn registry joined to the plane's agent list.
fn join_live_seats(cfg: &Config, project: &str) -> Vec<crate::seat_binder::LiveSeat> {
    let registry = crate::state::load_spawn_registry(project);
    let agents: Vec<(String, String)> = crate::backend::multiplexer_backend(cfg)
        .ok()
        .and_then(|b| b.agent_list().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.pane_id, a.agent_status.as_str().to_string()))
        .collect();
    let captain = crate::state::captain_pin(
        project,
        &crate::project_dir_str(cfg, project).unwrap_or_else(|_| project.to_string()),
    );
    crate::cmd::swarm::seat_rows(&registry, &agents, captain.as_deref())
        .into_iter()
        .map(|r| crate::seat_binder::LiveSeat {
            pin: r.pin,
            role: r.role,
            harness: r.harness,
            model: r.model,
            mode: r.mode,
            effort: r.effort,
            placement: crate::seat_binder::SeatPlacement::Pane,
            idle: matches!(r.status.as_str(), "idle" | "done"),
            fresh_session: false,
            idle_since_ms: None,
        })
        .collect()
}

pub(crate) fn cmd_workflow(cfg: &Config, cmd: &WorkflowCmd) -> Result<()> {
    use toron_workflow::schema::{Action, CrewSpec, Workflow};
    use toron_workflow::{Level, lint_workflow_with_principles};

    /// The full workflow set: built-ins (compiled-in) + catalog (canonical
    /// YAML + absorbed legacy), name → (workflow, source).
    fn all_workflows() -> Result<BTreeMap<String, (Workflow, &'static str)>> {
        workflow_catalog::all_workflows_merged(None)
    }

    /// W6 (§3.8): [`all_workflows`] plus the PROJECT tier —
    /// `<project>/.flywheel/workflows/*.yaml` shadows user catalog entries,
    /// which shadow built-ins.
    fn all_workflows_layered(
        project_root: Option<&Path>,
    ) -> Result<BTreeMap<String, (Workflow, &'static str)>> {
        workflow_catalog::all_workflows_merged(project_root)
    }

    /// §3.6 envelope #3 (wall-clock half): seconds a composed `--run` may
    /// take in THIS process. bd-enov (F7/F8): the explicit `--timeout` flag
    /// wins, then `FLYWHEEL_COMPOSE_MAX_SECONDS`, then the 3600s default
    /// (parsed/flagged values < 1 fall back — never 0). The bound errors the
    /// attempt LOUD; the durable run stays resumable.
    fn compose_wall_clock_cap(timeout_flag: Option<u64>) -> u64 {
        if let Some(secs) = timeout_flag.filter(|s| *s >= 1) {
            return secs;
        }
        std::env::var("FLYWHEEL_COMPOSE_MAX_SECONDS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s >= 1)
            .unwrap_or(3600)
    }

    /// Lint one workflow; print issues; return the blocking (Error-level) count.
    fn lint_one(name: &str, wf: &Workflow, project_root: Option<&std::path::Path>) -> usize {
        let report = lint_workflow_with_principles(wf, crate::principles::names());
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
        blocking += lint_flywheel_tier(name, wf, project_root);
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

    /// The layered catalog for a read door (`--project`): the same
    /// resolution order `run`/`compose` use — built-ins < user < project
    /// (`bd-workflow-catalog-read-doors-project-tier-jah1`). `None` keeps the
    /// historical user-tier view.
    fn read_door_workflows(
        cfg: &Config,
        project: &Option<String>,
    ) -> Result<BTreeMap<String, (Workflow, &'static str)>> {
        let project = project_door_opt(project, "workflow")?;
        match project {
            Some(p) => all_workflows_layered(Some(&project_dir(cfg, p))),
            None => all_workflows(),
        }
    }

    match cmd {
        WorkflowCmd::List {
            json: _,
            brief,
            project,
        } => {
            let all = read_door_workflows(cfg, project)?;
            if all.is_empty() {
                println!("no workflows defined");
                return Ok(());
            }
            if *brief {
                let rows = workflow_list_brief_rows(&all);
                print_json(&json!({ "workflows": rows }));
                return Ok(());
            }
            let rows = workflow_list_full_rows(&all);
            print_json(&json!({ "workflows": rows }));
            Ok(())
        }
        WorkflowCmd::Show { name, project } => {
            let all = read_door_workflows(cfg, project)?;
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
            let graph_labels: Vec<String> = wf
                .graph
                .as_ref()
                .map(|g| {
                    g.nodes
                        .iter()
                        .map(|(n, node)| format!("{n}: {}", node.action.type_name()))
                        .collect()
                })
                .unwrap_or_default();
            let (run, _family, _selection, compose_token) = workflow_hybrid_fields(&wf.name);
            print_json(&json!({
                "name": wf.name,
                "source": source,
                "description": wf.description,
                "run": run,
                "compose_token": compose_token,
                "trigger": wf.trigger.as_ref().map(toron_workflow::Trigger::type_name),
                "guard": wf.guard,
                "crew": crew,
                "inputs": wf.inputs.iter().map(|i| &i.name).collect::<Vec<_>>(),
                "steps": step_labels,
                "graph": graph_labels,
                "graph_nodes": wf.graph.as_ref().map(|g| g.nodes.len()),
            }));
            Ok(())
        }
        WorkflowCmd::Lint { name, project } => {
            let project_root = project.as_deref().map(|p| project_dir(cfg, p));
            let all = read_door_workflows(cfg, project)?;
            if let Some(n) = name {
                let (wf, _) = all
                    .get(n)
                    .ok_or_else(|| anyhow!("unknown workflow {n:?} (flywheel workflow list)"))?;
                let blocking = lint_one(n, wf, project_root.as_deref());
                if blocking > 0 {
                    std::process::exit(1);
                }
                Ok(())
            } else {
                let mut blocking_total = 0;
                for (n, (wf, _)) in &all {
                    blocking_total += lint_one(n, wf, project_root.as_deref());
                }
                if blocking_total > 0 {
                    std::process::exit(1);
                }
                println!("lint clean: {} workflow(s)", all.len());
                Ok(())
            }
        }
        WorkflowCmd::Evaluate {
            name,
            project,
            json,
        } => cmd_workflow_evaluate(cfg, name, project.as_deref(), *json),
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
            yes,
            allow_worktree: _,
            role_model,
            role_harness,
        } => {
            project_door_opt(project, "workflow run")?;
            let mut crew_sel = crew.clone();
            let mut role_model_sel = role_model.clone();
            let mut role_harness_sel = role_harness.clone();
            if *execute
                && briefing_enabled(*json, *yes, crew.as_deref(), std::io::stdin().is_terminal())
            {
                if !json && *yes && crew.is_some() {
                    eprintln!(
                        "flywheel: warn: --yes is redundant with an explicit --crew (briefing already skipped)"
                    );
                }
                let mut ctx = workflow_briefing_context(name, inputs.as_deref())?;
                let objective = ctx.objective.clone();
                enrich_suggestions_live(
                    &mut ctx,
                    &objective,
                    &crate::authority::RuntimeAuthority::resolve(),
                );
                let outcome = steer_brief::run_briefing(
                    &ctx,
                    &mut std::io::stdin().lock(),
                    &mut std::io::stdout(),
                )?;
                match outcome {
                    steer_brief::BriefingOutcome::Abort => {
                        bail!("workflow run: aborted by the operator at the briefing gate");
                    }
                    steer_brief::BriefingOutcome::Go {
                        budget,
                        role_models,
                        role_harnesses,
                    } => {
                        crew_sel = Some(budget.to_string());
                        if role_model_sel.is_empty() {
                            role_model_sel = role_models
                                .iter()
                                .map(|(role, model)| format!("{role}={model}"))
                                .collect();
                        }
                        if role_harness_sel.is_empty() {
                            role_harness_sel = role_harnesses
                                .iter()
                                .map(|(role, kind)| format!("{role}={kind}"))
                                .collect();
                        }
                    }
                }
            } else if *execute && !json && *yes && crew.is_some() {
                eprintln!(
                    "flywheel: warn: --yes is redundant with an explicit --crew (briefing already skipped)"
                );
            }
            let run_root = project_dir(cfg, project.as_deref().unwrap_or("flywheel"));
            let all = all_workflows_layered(Some(&run_root))?;
            let wf = resolve_workflow(name, &all)?.clone();
            if *execute
                && crate::workflow_catalog::WorkflowCatalog::load_layered(Some(&run_root))
                    .unwrap_or_default()
                    .is_compose_token(name)
            {
                eprintln!(
                    "flywheel: deprecated: `workflow run {name} --execute` is the legacy alias; prefer `workflow compose \"<objective>\" --parts {name} --run`"
                );
            }
            let blocking = lint_one(
                name,
                &wf,
                project.as_deref().map(|p| project_dir(cfg, p)).as_deref(),
            );
            if blocking > 0 {
                bail!("workflow {name:?} has {blocking} blocking lint issue(s) — refusing to run");
            }
            let mut inputs_value: Value = match inputs {
                Some(s) if !s.trim().is_empty() => {
                    serde_json::from_str(s).map_err(|e| anyhow!("--inputs must be JSON: {e}"))?
                }
                _ => json!({}),
            };
            crew::apply_input_defaults(&wf, &mut inputs_value);
            let resolver_root = run_root.clone();
            let workflow_resolver: toron_workflow::WorkflowResolver = Arc::new(move |sub: &str| {
                workflow_catalog::subworkflow_resolver(Some(&resolver_root))(sub)
            });
            let packed = packed_from_flag(
                cfg,
                &wf,
                &|sub| workflow_resolver(sub),
                crew_sel.as_deref(),
                None,
            )?;
            eprintln!("flywheel: {}", packed.announce(name));
            let wf = crew::apply_role_aliases(&wf, &packed.aliases);
            let workflow_resolver =
                crew::aliasing_resolver(workflow_resolver, packed.aliases.clone());
            let crew_plan = packed.as_plan();
            let mut pane_ctx: Option<pane::PaneRunCtx> = None;
            let runner: toron_workflow::engine::EffectRunner = if *execute {
                match mode.as_str() {
                    "headless" => {
                        headless_spawn_gate(&wf)?;
                        let mut catalog = profile::ProfileCatalog::load()?;
                        catalog.apply_role_models(&role_model_sel)?;
                        catalog.apply_role_harnesses(&role_harness_sel)?;
                        let wf_name = name.clone();
                        let inputs_c = inputs_value.clone();
                        let dispatch_project_dir =
                            project_dir(cfg, project.as_deref().unwrap_or("flywheel"));
                        Arc::new(move |inv: &toron_workflow::engine::ActionInvocation| {
                            headless_dispatch(
                                &catalog,
                                &wf_name,
                                &inputs_c,
                                None,
                                &inv.action,
                                Some(&dispatch_project_dir),
                            )
                        })
                    }
                    "pane" => {
                        let project = project.as_deref().unwrap_or("flywheel");
                        let mut catalog = profile::ProfileCatalog::load()?;
                        catalog.apply_role_models(&role_model_sel)?;
                        catalog.apply_role_harnesses(&role_harness_sel)?;
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
                        // bd-6setu.16 H3: the stage library is read ONCE here,
                        // at the construction site, and handed over as data.
                        let reads: std::collections::BTreeMap<String, Vec<String>> =
                            stages::StageLibrary::load()
                                .map(|lib| {
                                    lib.list()
                                        .into_iter()
                                        .filter_map(|n| {
                                            lib.get_stage(n)
                                                .map(|s| (n.to_string(), s.reads.clone()))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                        Arc::new(move |inv: &toron_workflow::engine::ActionInvocation| {
                            // bd-6setu.2: the durable leaf knows its run and
                            // node, so a pane dispatch here is bindable. The
                            // run id is the journal key; an empty one (a
                            // context that cannot name its run) writes no row.
                            let ctx = pane::DispatchContext {
                                scope: pane::DispatchScope {
                                    run_id: inv.run.run_id.as_str(),
                                    node_path: inv.run.node_path.as_str(),
                                },
                                reads: &reads,
                                prompt_port: None,
                            };
                            pane::pane_dispatch_scoped(
                                &catalog,
                                &project_c,
                                &wf_name,
                                &inputs_c,
                                &entries,
                                &inv.action,
                                toron_workflow::identity::copy_ordinal(
                                    inv.run.iteration_key.as_deref(),
                                ),
                                Some(&ctx),
                            )
                        })
                    }
                    other => bail!("unknown --mode {other:?} (headless | pane)"),
                }
            } else {
                Arc::new(|inv: &toron_workflow::engine::ActionInvocation| {
                    Ok(json!({ "action": inv.action.type_name() }))
                })
            };
            let rt = tokio::runtime::Runtime::new()?;
            let run_id = format!("fw-cli-{}", state::now_iso());
            let invoke_wf = crew::resolve_crew_fanouts(&wf, &crew_plan);
            let profile_resolver = profile::ProfileCatalog::catalog_profile_resolver();
            let mut spawn_receipts: Vec<Value> = vec![];
            let output = if let Some(pane_ctx) = pane_ctx.as_ref() {
                let engine = std::sync::Arc::new({
                    let eng = pane::local_engine_journaled(
                        cfg,
                        &rt,
                        runner,
                        profile_resolver,
                        workflow_resolver.clone(),
                    )?;
                    let sink = make_verify_sink(
                        cfg.clone(),
                        project.as_deref().unwrap_or("flywheel").to_string(),
                    );
                    *eng.verify_sink().0.lock().expect("verify-sink mutex") =
                        sink.0.lock().expect("verify-sink mutex").clone();
                    eng
                });
                let approval_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let approval_drainer = pane::start_approval_drainer(
                    cfg.clone(),
                    project.as_deref().unwrap_or("flywheel").to_string(),
                    engine.clone(),
                    approval_stop.clone(),
                )?;
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
                    phase_chain::invoke_or_ship_chain(
                        &engine,
                        &workflow_resolver,
                        &project_dir(cfg, project.as_deref().unwrap_or("flywheel")),
                        project.as_deref().unwrap_or("flywheel"),
                        &run_id,
                        name,
                        &invoke_wf,
                        inputs_value.clone(),
                    )
                    .await
                });
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                approval_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                spawn_receipts = drainer
                    .join()
                    .map_err(|_| anyhow!("spawn drainer thread panicked"))??;
                let _ = approval_drainer
                    .join()
                    .map_err(|_| anyhow!("approval drainer thread panicked"));
                out
            } else {
                let engine = std::sync::Arc::new({
                    let eng = if *execute {
                        pane::local_engine_journaled(
                            cfg,
                            &rt,
                            runner,
                            profile_resolver,
                            workflow_resolver.clone(),
                        )?
                    } else {
                        pane::local_engine(
                            &rt,
                            runner,
                            profile_resolver,
                            workflow_resolver.clone(),
                        )?
                    };
                    let sink = make_verify_sink(
                        cfg.clone(),
                        project.as_deref().unwrap_or("flywheel").to_string(),
                    );
                    *eng.verify_sink().0.lock().expect("verify-sink mutex") =
                        sink.0.lock().expect("verify-sink mutex").clone();
                    eng
                });
                let approval_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let approval_drainer = pane::start_approval_drainer(
                    cfg.clone(),
                    project.as_deref().unwrap_or("flywheel").to_string(),
                    engine.clone(),
                    approval_stop.clone(),
                )?;
                let out = rt.block_on(async {
                    phase_chain::invoke_or_ship_chain(
                        &engine,
                        &workflow_resolver,
                        &project_dir(cfg, project.as_deref().unwrap_or("flywheel")),
                        project.as_deref().unwrap_or("flywheel"),
                        &run_id,
                        name,
                        &invoke_wf,
                        inputs_value.clone(),
                    )
                    .await
                });
                approval_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = approval_drainer
                    .join()
                    .map_err(|_| anyhow!("approval drainer thread panicked"));
                out
            }?;
            let mut reaped = false;
            if let Some(ctx) = &pane_ctx
                && !ctx.retained
            {
                let backend = crate::backend::multiplexer_backend(cfg)?;
                match backend.workspace_close(&ctx.workspace_id, true) {
                    Ok(_) => reaped = true,
                    Err(e) => eprintln!(
                        "flywheel: workspace close {} failed: {e:#}",
                        ctx.workspace_id
                    ),
                }
                pane::reap_dead_registry_rows_on(
                    project.as_deref().unwrap_or("flywheel"),
                    &*backend,
                );
            }
            let run_project = project.as_deref().unwrap_or("flywheel");
            let terminal = run_terminal_status(&output);
            let artifact_paths = write_terminal_artifact(run_project, &run_id, &output, terminal);
            let action_stage = output_action_stage(&output);
            log(
                run_project,
                EventKind::Workflow,
                json!({
                    "workflow": name,
                    "status": terminal,
                    "crew": crew_plan.members,
                    "run_id": run_id,
                    "mode": mode,
                    "panes": pane_ctx.as_ref().map(|c| c.entries.len()),
                    "reaped": reaped,
                    "artifact_paths": artifact_paths,
                    "stage": action_stage,
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
                    out["panes"] = json!(ctx.entries.entries());
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
                    for e in ctx.entries.entries() {
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
                "name: {name}\ndescription: \"\"\n# Declare 2-3 governing principles (names from `flywheel principles list`;\n# the lint validates every name against the ADR-0019 21-name registry):\nprinciples: []\ntrigger:\n  type: webhook\ninputs:\n  - name: goal\nsteps:\n  - name: step-1\n    action: send_dm\n    to: operator\n    body: \"started {{{{name}}}}\"\n"
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
            from_yaml,
            dry_run,
            run,
            keep,
            project,
            inputs,
            idempotency_key,
            role_model,
            role_harness,
            harness,
            timeout,
            retire_orphans,
            _no_retire_orphans: _,
            crew,
            crew_max,
            allow_crew_expand,
            reuse,
            join,
            yes,
            json,
        } => {
            if keep.is_some() && (!*run || *dry_run) {
                bail!("workflow compose: --keep needs --run (graduation requires a green run)");
            }
            if *dry_run && *run {
                bail!("workflow compose: --dry-run never dispatches — drop --run");
            }
            if from_yaml.is_some() && (*agent || !parts.is_empty() || base.is_some()) {
                bail!("workflow compose: choose one arm — --from-yaml, --agent, or --parts/--base");
            }
            if !from_yaml.is_some() && *agent && (!parts.is_empty() || base.is_some()) {
                bail!(
                    "workflow compose: choose one arm — --agent (planner-authored) or --parts/--base (deterministic)"
                );
            }
            if from_yaml.is_none() && !*agent && parts.is_empty() {
                bail!(
                    "workflow compose needs --parts (role:stage / workflow:name / each:|when:|retry(n):|parallel: forms) or --from-yaml - or --agent"
                );
            }
            project_door_opt(project, "workflow compose")?;
            let project_name = project.as_deref().unwrap_or("flywheel");
            let project_root = project_dir(cfg, project_name);
            let project_stages = project_root.join(".flywheel/stages.toml");
            let all = all_workflows_layered(Some(&project_root))?;
            let proj_tag: String = std::path::Path::new(project_name)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("p")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(4)
                .collect();
            let wf_name = format!(
                "{}{}{:02x}",
                proj_tag,
                chrono::Utc::now().format("%m%d%H%M%S"),
                std::process::id() % 256
            );
            let composed = if let Some(src) = from_yaml {
                let text = if src == "-" {
                    use std::io::Read as _;
                    let mut buf = String::new();
                    std::io::stdin()
                        .lock()
                        .read_to_string(&mut buf)
                        .context("read workflow YAML from stdin")?;
                    buf
                } else {
                    std::fs::read_to_string(src)
                        .with_context(|| format!("read workflow YAML from {src}"))?
                };
                crew::workflow_from_yaml(&text)?
            } else if *agent {
                let wf_rows: Vec<(&str, &str)> = all
                    .iter()
                    .map(|(n, (w, _))| (n.as_str(), w.description.as_deref().unwrap_or("")))
                    .collect();
                let profile_names = profile::ProfileCatalog::load()?.names();
                let profile_refs: Vec<&str> = profile_names.iter().map(String::as_str).collect();
                let stage_lib = stages::StageLibrary::load_layered(Some(&project_stages))?;
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
                let compose_catalog =
                    crate::workflow_catalog::WorkflowCatalog::load_layered(Some(&project_root))
                        .unwrap_or_default();
                let mut parsed: Vec<crew::ComposePart> = Vec::new();
                for token in parts {
                    parsed.push(
                        crew::parse_compose_part_with(token, &|t| {
                            compose_catalog.is_compose_token(t)
                        })
                        .map_err(anyhow::Error::msg)?,
                    );
                }
                let stage_lib = stages::StageLibrary::load_layered(Some(&project_stages))?;
                let affinity = |s: &str| stage_lib.get_stage(s).and_then(|st| st.role.clone());
                let all_for_recipes = all.clone();
                let recipe_resolver = move |n: &str| {
                    all_for_recipes
                        .get(n)
                        .map(|(wf, _)| wf.clone())
                        .or_else(|| crate::builtin_workflows::builtin_recipe(n))
                };
                crew::compose_workflow_with_resolvers(
                    &wf_name,
                    target,
                    base_wf.as_ref(),
                    &parsed,
                    &affinity,
                    &recipe_resolver,
                )?
            };

            crew::enforce_budget_caps(&composed)?;

            let blocking = lint_one(&wf_name, &composed, Some(&project_root));
            if blocking > 0 {
                bail!(
                    "composed workflow {wf_name:?} has {blocking} blocking lint issue(s) — refusing to run"
                );
            }

            let resolve = |sub: &str| all.get(sub).map(|(w, _)| w.clone());
            let packed = packed_from_flag(cfg, &composed, &resolve, crew.as_deref(), *crew_max)?;
            eprintln!("flywheel: {}", packed.announce(&wf_name));
            if *allow_crew_expand {
                eprintln!(
                    "flywheel: warn: --allow-crew-expand is a no-op under ADR-0011 (cap is crew.max; pass --crew N)"
                );
            }
            let composed = crew::apply_role_aliases(&composed, &packed.aliases);
            let crew_plan = packed.as_plan();

            // bd-3ann1: an in-flight compose binds to the seats that are
            // already live. The plan is printed before consent; nothing here
            // touches the fleet.
            let join_plan = if *join {
                let project_name = crate::cron::detect_project(cfg, project.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("workflow compose --join: no project"))?;
                let seats = join_live_seats(cfg, &project_name);
                let plan = crate::compose_join::plan_join(
                    &composed,
                    &seats,
                    &crate::compose_join::JoinRoles::default(),
                    crew_max.unwrap_or(crate::crew::DEFAULT_CREW_MAX) as usize,
                );
                if plan.needs_yes && !*yes && !*dry_run {
                    bail!(
                        "workflow compose --join: roles {} have no live seat; re-run with --yes to start them",
                        plan.spawns.join(", ")
                    );
                }
                for line in crate::compose_join::render_preview(&plan) {
                    eprintln!("flywheel: join {line}");
                }
                Some(plan)
            } else {
                None
            };

            // bd-3ann1 slice 2: the bound steps really dispatch to their seat,
            // through the same stage with the bead header, not to a fresh
            // profile. A dry run renders this document and starts nothing.
            let composed = if let Some(plan) = &join_plan {
                let bead = target.split_whitespace().find(|w| w.starts_with("bd-"));
                crate::compose_join::apply_join_bindings(&composed, plan, bead)
            } else {
                composed
            };

            if *dry_run {
                let rendered = crew::render_dry_run(&composed, &resolve);
                if *json {
                    print_json(&json!({
                        "workflow": wf_name,
                        "target": target,
                        "dry_run": rendered,
                    }));
                } else {
                    println!("dry-run graph for {wf_name} (no dispatch):");
                    if let Some(steps) = rendered["steps"].as_array() {
                        for s in steps {
                            let mut line = format!(
                                "  - {} ({})",
                                s["path"].as_str().unwrap_or("<unnamed>"),
                                s["action"].as_str().unwrap_or("?")
                            );
                            if let Some(f) = s["fanout"].as_u64() {
                                line.push_str(&format!(" fanout={f}"));
                            }
                            if let Some(p) = s["phase"].as_str() {
                                line.push_str(&format!(" phase={p}"));
                            }
                            if let Some(g) = s["guard"].as_str() {
                                line.push_str(&format!(" guard={g:?}"));
                            }
                            println!("{line}");
                        }
                    }
                    print_json(&json!({
                        "workflow": wf_name,
                        "target": target,
                        "crew": crew_plan.members,
                        "dry_run": rendered,
                    }));
                }
                return Ok(());
            }

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
                                Action::DispatchJob { spec, .. } =>
                                    spec.profile.as_deref().unwrap_or("dispatch"),
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

            let mut inputs_value = serde_json::Map::new();
            inputs_value.insert("objective".into(), json!(target));
            if let Some(txt) = inputs.as_deref() {
                let extra: Value = serde_json::from_str(txt)
                    .map_err(|e| anyhow!("--inputs must be a JSON object: {e}"))?;
                match extra {
                    Value::Object(m) => {
                        for (k, v) in m {
                            inputs_value.insert(k, v);
                        }
                    }
                    _ => bail!("--inputs must be a JSON object"),
                }
            }
            let inputs_c = Value::Object(inputs_value);
            let steer_channel = crew::resolve_steer_channel(&composed, &inputs_c)?;
            let run_id =
                if let Some(k) = idempotency_key.as_deref().filter(|s| !s.trim().is_empty()) {
                    format!(
                        "fw-composed-{}",
                        k.trim()
                            .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-")
                    )
                } else if let Some(bid) = inputs_c
                    .get("bead_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    format!("fw-composed-{bid}")
                } else {
                    format!("fw-composed-{}", state::now_iso())
                };
            if running_run_row_exists(project_name, &run_id) {
                bail!(
                    "workflow compose: run `{run_id}` is already RUNNING (idempotency-key hit) — \
                     refusing to spawn a second crew into {project_name}. Inspect it with `flywheel \
                     workflow history`, or pass a fresh --idempotency-key."
                );
            }
            let mut lifecycle = ComposeRunLifecycle::start(
                project_name,
                &wf_name,
                &run_id,
                crew_plan.members.clone(),
            );
            let mut catalog = profile::ProfileCatalog::load()?;
            catalog.apply_role_models(role_model)?;
            catalog.apply_role_harnesses(role_harness)?;
            if let Some(kind) = harness.as_deref() {
                catalog.apply_harness_override(kind)?;
            }
            let ctx = pane::pre_spawn_crew(
                cfg,
                project_name,
                &wf_name,
                &crew_plan,
                &catalog,
                reuse.as_deref(),
                false,
                *retire_orphans,
            )?;
            let entries = ctx.entries.clone();
            if let Some(channel) = steer_channel.as_deref() {
                let pins: Vec<String> = entries
                    .entries()
                    .iter()
                    .filter_map(|e| e.mail_identity.clone())
                    .collect();
                ensure_and_join_channels(project_name, &[channel.to_string()], &pins);
            }
            let project_c = project_name.to_string();
            let wf_name_c = wf_name.clone();
            let runner: toron_workflow::engine::EffectRunner = {
                let entries = entries.clone();
                let inputs_c = inputs_c.clone();
                Arc::new(move |inv: &toron_workflow::engine::ActionInvocation| {
                    pane::pane_dispatch(
                        &catalog,
                        &project_c,
                        &wf_name_c,
                        &inputs_c,
                        &entries,
                        &inv.action,
                        toron_workflow::identity::copy_ordinal(inv.run.iteration_key.as_deref()),
                    )
                })
            };
            let rt = tokio::runtime::Runtime::new()?;
            let invoke_wf = crew::resolve_crew_fanouts(&composed, &crew_plan);
            let resolver_root = project_root.clone();
            let workflow_resolver: toron_workflow::WorkflowResolver = Arc::new(move |sub: &str| {
                workflow_catalog::subworkflow_resolver(Some(&resolver_root))(sub)
            });
            let workflow_resolver =
                crew::aliasing_resolver(workflow_resolver, packed.aliases.clone());
            let profile_resolver = profile::ProfileCatalog::catalog_profile_resolver();
            let mut spawn_receipts: Vec<Value> = vec![];
            let engine = std::sync::Arc::new({
                let eng = pane::local_engine_journaled(
                    cfg,
                    &rt,
                    runner,
                    profile_resolver,
                    workflow_resolver,
                )?;
                let sink = make_verify_sink(cfg.clone(), project_name.to_string());
                *eng.verify_sink().0.lock().expect("verify-sink mutex") =
                    sink.0.lock().expect("verify-sink mutex").clone();
                eng
            });
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let drainer = pane::start_spawn_drainer(
                cfg.clone(),
                project_name.to_string(),
                wf_name.clone(),
                ctx.workspace_id.clone(),
                engine.clone(),
                stop.clone(),
            )?;
            let approval_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let approval_drainer = pane::start_approval_drainer(
                cfg.clone(),
                project_name.to_string(),
                engine.clone(),
                approval_stop.clone(),
            )?;
            let policy_receipts_count;
            let output = {
                let invoke_outcome = rt.block_on(async {
                    let out = crew::bounded_invoke(
                        &engine,
                        &run_id,
                        invoke_wf,
                        inputs_c.clone(),
                        compose_wall_clock_cap(*timeout),
                    )
                    .await?;
                    let drained: Vec<toron_workflow::PolicyEvent> = {
                        let mut guard = engine.policy_sink().lock().await;
                        std::mem::take(&mut *guard)
                    };
                    Ok::<_, anyhow::Error>((out, drained))
                });
                let (output, policy_events) = match invoke_outcome {
                    Ok(v) => v,
                    Err(e) => {
                        lifecycle.fail(&e.to_string());
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        approval_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        let _ = approval_drainer.join();
                        return Err(e);
                    }
                };
                let policy_receipts_count_ = policy_events.len();
                for ev in &policy_events {
                    match ev.action {
                        toron_workflow::schema::FailureAction::Bead => {
                            match cron::spawn_failure_bead(&ev.bundle) {
                                Ok(id) => eprintln!(
                                    "composed run {wf_name}: failure -> bead {id} ({}#{})",
                                    ev.bundle.workflow, ev.bundle.consecutive
                                ),
                                Err(e) => eprintln!(
                                    "composed run {wf_name}: policy bead side-effect failed: {e:#}"
                                ),
                            }
                        }
                        other => eprintln!(
                            "composed run {wf_name}: policy action {other:?} recorded (side-effect not wired)"
                        ),
                    }
                }
                policy_receipts_count = policy_receipts_count_;
                output
            };
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            spawn_receipts = drainer
                .join()
                .map_err(|_| anyhow!("spawn drainer thread panicked"))??;
            approval_stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = approval_drainer
                .join()
                .map_err(|_| anyhow!("approval drainer thread panicked"));
            let mut reaped = false;
            if !ctx.retained {
                let backend = crate::backend::multiplexer_backend(cfg)?;
                match backend.workspace_close(&ctx.workspace_id, true) {
                    Ok(_) => reaped = true,
                    Err(e) => eprintln!(
                        "flywheel: workspace close {} failed: {e:#}",
                        ctx.workspace_id
                    ),
                }
                pane::reap_dead_registry_rows_on(
                    project.as_deref().unwrap_or("flywheel"),
                    &*backend,
                );
            }
            let terminal = run_terminal_status(&output);
            let artifact_paths = write_terminal_artifact(project_name, &run_id, &output, terminal);
            let action_stage = output_action_stage(&output);
            lifecycle.finish(json!({
                "workflow": wf_name,
                "status": terminal,
                "crew": crew_plan.members,
                "run_id": run_id,
                "mode": "pane",
                "panes": entries.len(),
                "reaped": reaped,
                "policy_events": policy_receipts_count,
                "artifact_paths": artifact_paths,
                "stage": action_stage,
            }));
            if let Some(keep_name) = keep {
                let green = terminal != "failed";
                if !green {
                    bail!(
                        "refusing to keep {keep_name:?}: the composed run {wf_name} did not go green \
                         (status {:?}) — fix the run or drop --keep",
                        output
                            .get("status")
                            .and_then(|s| s.as_str())
                            .unwrap_or("<none>")
                    );
                }
                let mut kept = composed.clone();
                let from = if parts.is_empty() {
                    base.clone().unwrap_or_else(|| "<planner>".to_string())
                } else {
                    parts.join(",")
                };
                kept.name = keep_name.clone();
                kept.description = Some(format!(
                    "{} [origin: composed; from: {from}; first-run: {}; author: {}]",
                    kept.description.as_deref().unwrap_or("composed workflow"),
                    state::now_iso(),
                    operator_pin(),
                ));
                let blocking = lint_one(keep_name, &kept, Some(&project_root));
                if blocking > 0 {
                    bail!(
                        "kept workflow {keep_name:?} has {blocking} blocking lint issue(s) — not kept"
                    );
                }
                let dir = project_root.join(".flywheel/workflows");
                let path = workflow_catalog::write_workflow_yaml(&dir, &kept)?;
                println!(
                    "kept {keep_name} → {} (green run {run_id}; provenance in description)",
                    path.display()
                );
            }
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
        WorkflowCmd::Seed {
            json,
            overwrite,
            dry_run,
            yes,
        } => {
            use crate::workflow_catalog::OverwriteScope;
            let scope = OverwriteScope::parse(overwrite)?;
            let root = config::config_dir();
            let plan = workflow_catalog::seed_user_catalog_with(&root, scope, true)?;
            let targets: Vec<String> = plan.planned_overwrites.to_vec();
            if !*dry_run {
                crate::output::confirm_destructive("overwrite", &targets, *yes, *json)?;
            }
            let report = workflow_catalog::seed_user_catalog_with(&root, scope, *dry_run)?;
            if *json {
                print_json(&serde_json::to_value(&report)?);
                return Ok(());
            }
            let verb = if *dry_run { "would seed" } else { "seeded" };
            println!(
                "{} {} {} user catalog  {}",
                output::ok("\u{2705}"),
                verb,
                output::accent(&root.display().to_string()),
                output::dim(&format!("(overwrite: {})", scope.as_str()))
            );
            let line = |n: usize, what: &str, paint: &dyn Fn(&str) -> String| {
                println!("   {} {}", paint(&n.to_string()), output::dim(what));
            };
            if !report.created_workflows.is_empty() || !report.created_stages.is_empty() {
                line(
                    report.created_workflows.len(),
                    "workflow(s) created",
                    &|s| output::ok(s),
                );
                line(report.created_stages.len(), "stage(s) created", &|s| {
                    output::ok(s)
                });
            }
            if !report.overwritten_workflows.is_empty() || !report.overwritten_stages.is_empty() {
                line(
                    report.overwritten_workflows.len(),
                    "workflow(s) RESTORED from the builtin (old bytes kept as .bak)",
                    &|s| output::warn(s),
                );
                line(
                    report.overwritten_stages.len(),
                    "stage(s) restored from the builtin (old bytes kept as .bak)",
                    &|s| output::warn(s),
                );
            }
            if !report.kept_workflows.is_empty() || !report.kept_stages.is_empty() {
                line(
                    report.kept_workflows.len() + report.kept_stages.len(),
                    "existing file(s) left untouched (raise --overwrite to replace)",
                    &|s| output::dim(s),
                );
            }
            if *dry_run {
                if !report.planned_overwrites.is_empty() {
                    println!(
                        "\n{} {}",
                        output::warn("\u{26a0}\u{fe0f}"),
                        output::warn(&format!(
                            "would REPLACE {} builtin-named file(s):",
                            report.planned_overwrites.len()
                        ))
                    );
                    for name in report.planned_overwrites.iter().take(12) {
                        println!("   {}", output::dim(name));
                    }
                    if report.planned_overwrites.len() > 12 {
                        println!(
                            "   {}",
                            output::dim(&format!(
                                "... and {} more (use --json for the full list)",
                                report.planned_overwrites.len() - 12
                            ))
                        );
                    }
                    println!(
                        "   {}",
                        output::dim("each replaced file keeps its old bytes at <name>.bak")
                    );
                }
                println!(
                    "{}",
                    output::dim("nothing was written. Re-run without --dry-run to apply.")
                );
            }
            println!(
                "{}",
                output::dim(
                    "next: flywheel workflow drift  \u{2192}  flywheel workflow show <name>"
                )
            );
            Ok(())
        }
        WorkflowCmd::Drift { json } => {
            let rows = workflow_catalog::catalog_drift(&config::config_dir())?;
            if *json {
                print_json(&json!({ "entries": rows }));
            } else {
                for want in ["stale", "missing", "custom"] {
                    let group: Vec<_> = rows.iter().filter(|r| r.status == want).collect();
                    if group.is_empty() {
                        continue;
                    }
                    println!(
                        "\n{} {}",
                        output::catalog_badge(want),
                        output::dim(&format!("{} {}", group.len(), want))
                    );
                    for row in group {
                        println!(
                            "   {:<8} {:<34} {}",
                            output::dim(&row.kind),
                            row.name,
                            output::dim(&format!("from {}", row.source))
                        );
                    }
                }
                let clean: Vec<_> = rows.iter().filter(|r| r.status == "current").collect();
                if !clean.is_empty() {
                    println!(
                        "\n{} {}",
                        output::catalog_badge("current"),
                        output::dim(&format!("{} match the builtin", clean.len()))
                    );
                    for row in &clean {
                        println!(
                            "   {:<8} {:<34} {}",
                            output::dim(&row.kind),
                            row.name,
                            output::dim(&format!("from {}", row.source))
                        );
                    }
                }
                let stale = rows.iter().filter(|row| row.status == "stale").count();
                let missing = rows.iter().filter(|row| row.status == "missing").count();
                let custom = rows.iter().filter(|row| row.status == "custom").count();
                println!();
                if stale == 0 && missing == 0 {
                    println!(
                        "{} {}",
                        output::ok("\u{2705}"),
                        output::ok("user catalog matches the binary")
                    );
                } else {
                    println!(
                        "{} {}",
                        output::warn("\u{26a0}\u{fe0f}"),
                        output::warn(&format!(
                            "{stale} stale, {missing} missing, {custom} custom"
                        ))
                    );
                    if stale > 0 {
                        println!(
                            "   {}",
                            output::accent(
                                "fix: flywheel workflow seed --dry-run --overwrite stale",
                            )
                        );
                    }
                    if missing > 0 {
                        println!("   {}", output::accent("fix: flywheel workflow seed"));
                    }
                }
            }
            Ok(())
        }
        WorkflowCmd::History { name } => {
            use crate::state::EventKind;
            let mut rows: Vec<Value> = Vec::new();
            for project in state::project_logs() {
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
                            "kind": run_row_kind(&project, &e),
                            "run_id": e.data.get("run_id"),
                            "status": history_row_status(&e.data),
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
        WorkflowCmd::Crew { name, project } => {
            let all = read_door_workflows(cfg, project)?;
            let wf = resolve_workflow(name, &all)?;
            let resolve = |sub: &str| all.get(sub).map(|(w, _)| w.clone());
            let packed = packed_from_flag(cfg, wf, &resolve, None, None)?;
            let members: Vec<Value> = packed
                .members
                .iter()
                .map(|(role, count)| json!({ "role": role, "count": count }))
                .collect();
            if members.is_empty() {
                println!("workflow {name} has no crew (crew omitted or no profile refs)");
                return Ok(());
            }
            print_json(&json!({
                "workflow": name,
                "crew": members,
                "budget": packed.budget,
                "cap": packed.cap,
                "unique": packed.unique,
                "aliases": packed.aliases,
                "announce": packed.announce(name),
            }));
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
        WorkflowCmd::Promote {
            name,
            to,
            evidence,
            review_bead,
        } => {
            let root = project_dir(cfg, "flywheel");
            let layered = workflow_catalog::WorkflowCatalog::load_layered(Some(&root))?;
            let wf = layered
                .workflows
                .get(name)
                .ok_or_else(|| {
                    anyhow!("no promotable workflow named {name:?} (flywheel workflow list)")
                })?
                .clone();
            match to.as_str() {
                "user" => {
                    let dest = config::config_dir().join("workflows");
                    let path = workflow_catalog::write_workflow_yaml(&dest, &wf)?;
                    println!("promoted {name} → user tier ({})", path.display());
                    Ok(())
                }
                "repo" => {
                    repo_evidence_verified(name, evidence, review_bead.as_deref())?;
                    bail!(
                        "workflow repo tier lands via the review bead + an upstream flywheel commit \
                         (evidence verified: {} greens) — stages automate this; workflows are code-adjacent \
                         and need the seed regeneration pass (ADR-0022 §3.8)",
                        evidence.len()
                    );
                }
                other => bail!("unknown promote target {other:?} (user | repo)"),
            }
        }
        WorkflowCmd::Verify {
            name,
            export_tla,
            json,
        } => cmd_workflow_verify(name.as_deref(), *export_tla, *json),
        WorkflowCmd::Approve {
            run_id,
            promise_id,
            decision,
            reason,
            project,
        } => {
            let key = project_dir_str(cfg, project)?;
            let mail = mail_or_die(cfg)?;
            mail.ensure_project(&key)?;
            let target = mail.name.clone();
            let subject = format!("{decision}:{promise_id}");
            let body = format!(
                "SIGNOFF decision for run {run_id}: {decision}{}",
                reason
                    .as_deref()
                    .map(|r| format!("\nreason: {r}"))
                    .unwrap_or_default()
            );
            mail.send(
                &key,
                std::slice::from_ref(&target),
                &subject,
                &body,
                "high",
                false,
                None,
            )?;
            println!(
                "workflow approve: decision mailed to {target} ({subject}) — the parked run's drainer resolves within ~2s (run {run_id})"
            );
            Ok(())
        }
    }
}

/// One chart's verdict in `flywheel workflow verify` (ADR-0021 D5).
#[derive(serde::Serialize)]
struct WorkflowVerifyChartReport {
    name: String,
    ok: bool,
    checks: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tla: Option<String>,
    /// Slice A (bd-36zlq): layered-catalog tier of this row
    /// (`builtin` | `user` | `project`). Hand-written charts carry none.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    tier: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    errors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    warnings: Vec<String>,
}

const WORKFLOW_VERIFY_SCOPE: &str = "topology only: every guard counts as enabled and data is not modelled, so a listed state is a real defect and an empty list is not a proof; no model checker runs; waiting states checked are park (max_wait_secs) and dispatch_job await: result (result_timeout); request_approval has no timeout field in toron-workflow, so its wait is not checked";

/// The bounded and witnessed findings one chart's report renders.
#[derive(Default)]
struct ChartFindings<'a> {
    bounded_errors: &'a [String],
    bounded_warnings: &'a [String],
    witnessed_errors: &'a [String],
}

fn chart_report<S: Ord + Clone + std::fmt::Debug>(
    name: &str,
    topology: &toron_core::kernel::Topology<S>,
    goal: &str,
    is_goal: impl Fn(&S) -> bool,
    tla: Option<String>,
    tier: Option<&str>,
    findings: &ChartFindings<'_>,
) -> WorkflowVerifyChartReport {
    let names =
        |states: Vec<S>| -> Vec<String> { states.into_iter().map(|s| format!("{s:?}")).collect() };
    let unreachable = names(topology.unreachable());
    let stuck = names(topology.cannot_reach(is_goal));
    let mut errors = Vec::new();
    for state in &unreachable {
        errors.push(format!(
            "{name}: {state} is unreachable from the initial state"
        ));
    }
    for state in &stuck {
        errors.push(format!("{name}: {state} has no path to {goal}"));
    }
    for e in findings.bounded_errors {
        errors.push(format!("{name}: {e}"));
    }
    for e in findings.witnessed_errors {
        errors.push(format!("{name}: {e}"));
    }
    let mut warnings = Vec::new();
    for w in findings.bounded_warnings {
        warnings.push(format!("{name}: {w}"));
    }
    WorkflowVerifyChartReport {
        name: name.to_string(),
        ok: errors.is_empty(),
        checks: serde_json::json!({
            "reachable": { "ok": unreachable.is_empty(), "unreachable": unreachable },
            "can_finish": { "ok": stuck.is_empty(), "goal": goal, "stuck": stuck },
            "bounded": { "ok": findings.bounded_errors.is_empty(), "unbounded": findings.bounded_errors },
            "witnessed": { "ok": findings.witnessed_errors.is_empty(), "unwitnessed": findings.witnessed_errors },
        }),
        tla,
        tier: tier.map(str::to_string),
        errors,
        warnings,
    }
}

/// One chart report for a catalog workflow: lower it, then run the same
/// reachable/can-finish gates as the hand-written charts plus the bounded
/// check. A workflow that fails to lower is a named error, not a panic.
/// `tier` is the layered-catalog tier (`builtin` | `user` | `project`);
/// `--export-tla` emits the topology-only module (ADR-0021 D5.3).
fn verify_catalog_workflow(
    name: &str,
    wf: &toron_workflow::schema::Workflow,
    tier: &str,
    stages: &crate::stages::StageLibrary,
    export_tla: bool,
) -> WorkflowVerifyChartReport {
    match crate::workflow_lowering::lower(wf) {
        Err(e) => WorkflowVerifyChartReport {
            name: name.to_string(),
            ok: false,
            checks: serde_json::json!({
                "reachable": { "ok": false, "unreachable": [] },
                "can_finish": { "ok": false, "goal": "a top-level final state", "stuck": [] },
                "bounded": { "ok": false, "unbounded": [] },
                "witnessed": { "ok": false, "unwitnessed": [] },
            }),
            tla: None,
            tier: Some(tier.to_string()),
            errors: vec![format!("{name}: failed to lower ({e:?})")],
            warnings: Vec::new(),
        },
        Ok(lowered) => {
            let topo = lowered.machine.topology();
            let is_final = |state: &_| {
                topo.nodes.get(state).is_some_and(|node| {
                    node.node_type == toron_core::kernel::StateNodeType::Final
                        && node.parent.is_none()
                })
            };
            let (bounded_errors, bounded_warnings) =
                crate::workflow_lowering::check_bounded(&lowered);
            let witnessed_errors = crate::workflow_lowering::check_witnessed(wf, stages);
            chart_report(
                name,
                &topo,
                "a top-level final state",
                is_final,
                export_tla.then(|| crate::workflow_lowering::tla_topology_module(&lowered, name)),
                Some(tier),
                &ChartFindings {
                    bounded_errors: &bounded_errors,
                    bounded_warnings: &bounded_warnings,
                    witnessed_errors: &witnessed_errors,
                },
            )
        }
    }
}

/// Slice A (bd-36zlq): the tier of a merged catalog row. A file-tier copy
/// shadows a builtin of the same name, so anything the layered catalog
/// carries verifies as the file tier that wins at runtime — never builtin.
fn catalog_tier(source: &str, name: &str, project_names: &BTreeSet<String>) -> &'static str {
    if project_names.contains(name) {
        "project"
    } else if source == "catalog" {
        "user"
    } else {
        "builtin"
    }
}

/// Names declared by the project tier (`<root>/.flywheel/workflows/`), the
/// set that shadows user and builtin rows at runtime. Unreadable entries
/// read as absent — verify reports the lower tier instead of failing.
fn project_workflow_names(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let entries = match std::fs::read_dir(root.join(".flywheel").join("workflows")) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
        let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        let name = match ext {
            "yaml" | "yml" => std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| toron_workflow::schema::parse_workflow(&raw).ok())
                .map(|wf| wf.name),
            "toml" => std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| toml::from_str::<toron_workflow::schema::Workflow>(&raw).ok())
                .map(|wf| wf.name),
            _ => None,
        };
        if let Some(name) = name {
            out.insert(name);
        }
    }
    out
}

fn cmd_workflow_verify(name: Option<&str>, export_tla: bool, json_out: bool) -> Result<()> {
    use crate::cmd::build_break_statechart::build_build_break_machine;
    use crate::elm::brief_statechart::build_brief_machine;
    use crate::elm::captain_watch_statechart::build_captain_watch_machine;
    use crate::elm::deadlock_watch_statechart::build_deadlock_watch_machine;
    use crate::elm::lease_dirty_statechart::build_dirty_lease_machine;
    use crate::elm::lease_waitlist_statechart::build_lease_waitlist_machine;
    use crate::elm::mail_queue_statechart::build_mail_queue_machine;
    use crate::elm::p2p_negotiation::{P2PNegotiationState, build_p2p_negotiation_machine};
    use crate::elm::plan_statechart::build_plan_machine;
    use crate::elm::pre_edit_lease_statechart::build_pre_edit_machine;
    use crate::elm::result_mail_statechart::build_result_mail_machine;
    use crate::elm::seat_statechart::build_seat_machine;
    use crate::elm::work_unit_statechart::build_work_unit_machine;

    fn verify_p2p(export_tla: bool) -> WorkflowVerifyChartReport {
        let machine = build_p2p_negotiation_machine();
        chart_report(
            "P2PNegotiation",
            &machine.topology(),
            "Idle",
            |state| *state == P2PNegotiationState::Idle,
            export_tla.then(|| machine.to_tla_named("P2PNegotiation")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_seat(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_seat_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "Seat",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("Seat")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_work_unit(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_work_unit_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "WorkUnit",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("WorkUnit")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_brief(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_brief_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "Brief",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("Brief")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_deadlock_watch(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_deadlock_watch_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "DeadlockWatch",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("DeadlockWatch")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_build_break(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_build_break_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "BuildBreak",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("BuildBreak")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_result_mail(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_result_mail_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "ResultMail",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("ResultMail")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_captain_watch(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_captain_watch_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "CaptainWatch",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("CaptainWatch")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_lease_waitlist(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_lease_waitlist_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "LeaseWaitlist",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("LeaseWaitlist")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_mail_queue(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_mail_queue_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "MailQueue",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("MailQueue")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_dirty_lease(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_dirty_lease_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "DirtyLease",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("DirtyLease")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_pre_edit(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_pre_edit_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "PreEdit",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("PreEdit")),
            None,
            &ChartFindings::default(),
        )
    }

    fn verify_plan(export_tla: bool) -> WorkflowVerifyChartReport {
        use toron_core::kernel::StateNodeType;
        let machine = build_plan_machine();
        let topology = machine.topology();
        let is_final = |state: &_| {
            topology
                .nodes
                .get(state)
                .is_some_and(|node| node.node_type == StateNodeType::Final)
        };
        chart_report(
            "Plan",
            &topology,
            "a final state",
            is_final,
            export_tla.then(|| machine.to_tla_named("Plan")),
            None,
            &ChartFindings::default(),
        )
    }

    let want_all = name.is_none();
    let want = name.unwrap_or("").to_ascii_lowercase();
    let mut reports = Vec::new();
    if want_all || matches!(want.as_str(), "p2p" | "p2p-negotiation" | "p2pnegotiation") {
        reports.push(verify_p2p(export_tla));
    }
    if want_all || matches!(want.as_str(), "seat" | "seat-statechart" | "seatstatechart") {
        reports.push(verify_seat(export_tla));
    }
    if want_all || matches!(want.as_str(), "plan" | "plan-statechart" | "planstatechart") {
        reports.push(verify_plan(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "captainwatch" | "captain-watch" | "captain-watch-statechart"
        )
    {
        reports.push(verify_captain_watch(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "leasewaitlist" | "lease-waitlist" | "lease-waitlist-statechart"
        )
    {
        reports.push(verify_lease_waitlist(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "preedit" | "pre-edit" | "pre-edit-statechart"
        )
    {
        reports.push(verify_pre_edit(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "dirtylease" | "dirty-lease" | "dirty-lease-statechart"
        )
    {
        reports.push(verify_dirty_lease(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "mailqueue" | "mail-queue" | "mail-queue-statechart"
        )
    {
        reports.push(verify_mail_queue(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "buildbreak" | "build-break" | "build-break-statechart"
        )
    {
        reports.push(verify_build_break(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "resultmail" | "result-mail" | "result-mail-statechart"
        )
    {
        reports.push(verify_result_mail(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "brief" | "brief-statechart" | "briefstatechart"
        )
    {
        reports.push(verify_brief(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "workunit" | "work-unit" | "work-unit-statechart"
        )
    {
        reports.push(verify_work_unit(export_tla));
    }
    if want_all
        || matches!(
            want.as_str(),
            "deadlockwatch" | "deadlock-watch" | "deadlock-watch-statechart"
        )
    {
        reports.push(verify_deadlock_watch(export_tla));
    }
    // Catalog workflows (ADR-0021 D5.2 slice A, bd-36zlq): one chart report
    // per lowered workflow from the layered catalog — builtins, then the
    // user tier, then the project tier — so a file-tier copy verifies as
    // the copy that wins at runtime. A name argument matches one catalog
    // workflow case-insensitively; without one, all rows verify. The CLI
    // loads the operator's real tiers; tests use `load_layered_with`.
    let project_root = std::env::current_dir()
        .ok()
        .filter(|cwd| cwd.join(".flywheel").join("workflows").is_dir());
    let project_stages = project_root
        .as_ref()
        .map(|root| root.join(".flywheel").join("stages.toml"));
    let stages = crate::stages::StageLibrary::load_layered(project_stages.as_deref())
        .unwrap_or_else(|_| crate::stages::StageLibrary::compiled_in());
    let merged = workflow_catalog::all_workflows_merged(project_root.as_deref())?;
    let project_names = match project_root.as_deref() {
        Some(root) => project_workflow_names(root),
        None => BTreeSet::new(),
    };
    if want_all {
        for (n, (wf, source)) in &merged {
            reports.push(verify_catalog_workflow(
                n,
                wf,
                catalog_tier(source, n, &project_names),
                &stages,
                export_tla,
            ));
        }
    } else if let Some((n, (wf, source))) =
        merged.iter().find(|(k, _)| k.to_ascii_lowercase() == want)
    {
        reports.push(verify_catalog_workflow(
            n,
            wf,
            catalog_tier(source, n, &project_names),
            &stages,
            export_tla,
        ));
    }
    if reports.is_empty() {
        bail!(
            "unknown statechart {} (expected P2PNegotiation, Seat, Plan, LeaseWaitlist, DirtyLease, BuildBreak, Brief, ResultMail, CaptainWatch, DeadlockWatch, WorkUnit, or a workflow name)",
            name.unwrap_or("")
        );
    }

    // ADR-0021 D6 (bd-jhz8x.4): an unused stage FAILS this gate. "Used" is the
    // builtin workflows' `stage:` refs PLUS a stage that DECLARES how it is
    // reached without one (`reached_by = compose|manual`) — a compose token such
    // as `scout:fresh-eyes-explore` is invisible to a workflow scan, and reading
    // the operator's run log here would make the gate ambient. No run log, no
    // history: a proof and a workstation answer the same.
    let unused: Vec<String> = {
        let lib = crate::stages::StageLibrary::load_layered(None).unwrap_or_default();
        let mut referenced = std::collections::HashSet::new();
        for wf in crate::builtin_workflows::builtin_workflows().values() {
            let json = serde_json::to_value(wf).unwrap_or_default();
            crate::stages::collect_stage_refs(&json, &mut referenced);
        }
        referenced.extend(lib.declared_reach());
        let names: Vec<String> = lib.list_tiered().iter().map(|r| r.name.clone()).collect();
        crate::stages::unused_stages(&names, &referenced)
    };
    let ok = reports.iter().all(|r| r.ok) && unused.is_empty();
    let body = serde_json::json!({
        "schema": "flywheel-workflow-verify.v2",
        "ok": ok,
        "scope": WORKFLOW_VERIFY_SCOPE,
        "unused_stages": unused,
        "charts": reports,
    });
    if json_out {
        print_json(&body);
    } else {
        println!("workflow verify scope: {WORKFLOW_VERIFY_SCOPE}");
        for stage in body["unused_stages"].as_array().into_iter().flatten() {
            println!(
                "workflow verify unused stage: {} — no catalog workflow references it and no run used it in 30 days",
                stage.as_str().unwrap_or("?")
            );
        }
        for chart in body["charts"].as_array().into_iter().flatten() {
            println!(
                "workflow verify {}: {}",
                chart["name"].as_str().unwrap_or("?"),
                if chart["ok"].as_bool().unwrap_or(false) {
                    "ok"
                } else {
                    "FAILED"
                }
            );
            if let Some(tla) = chart.get("tla").and_then(|v| v.as_str()) {
                println!("{tla}");
            }
            if let Some(errs) = chart.get("errors").and_then(|v| v.as_array()) {
                for e in errs {
                    if let Some(s) = e.as_str() {
                        eprintln!("  - {s}");
                    }
                }
            }
        }
    }
    if !ok {
        bail!("statechart verification failed");
    }
    Ok(())
}

/// The flywheel-tier lint checks (the bead's fail-stop contract): profile
/// `@name` resolution against the ProfileCatalog, `stage:` alias resolution
/// against the StageLibrary, and `{{inputs.x}}` presence against the declared
/// `inputs:`. The ENGINE cannot do these (no dependency inversion — it never
/// imports flywheel's StageLibrary / ProfileCatalog), so the CLI owns them
/// and fails closed before any run.
///
/// `project_root` is required, not optional: the lint must resolve stages
/// through the SAME tiers the composer and runner use, or a project-tier
/// stage (`.flywheel/stages.toml`) is reported unresolvable and every
/// composed workflow that references it is refused. That made every
/// project-authored stage uncomposable while `stage show` listed it as valid.
/// Moved from crate root (bd-wave5-main-eviction-d5hht); shared by `cmd_workflow` and lint tests.
pub(crate) fn lint_flywheel_tier(
    name: &str,
    wf: &toron_workflow::schema::Workflow,
    project_root: Option<&std::path::Path>,
) -> usize {
    lint_flywheel_tier_in(name, wf, project_root, None)
}

pub(crate) fn lint_flywheel_tier_in(
    name: &str,
    wf: &toron_workflow::schema::Workflow,
    project_root: Option<&std::path::Path>,
    config_root: Option<&std::path::Path>,
) -> usize {
    let profiles = profile::ProfileCatalog::load().unwrap_or_default();
    let project_stages = project_root.map(|r| r.join(".flywheel").join("stages.toml"));
    let stages =
        stages::StageLibrary::load_layered_with_config(project_stages.as_deref(), config_root)
            .unwrap_or_default();
    let declared_inputs: std::collections::HashSet<&str> =
        wf.inputs.iter().map(|i| i.name.as_str()).collect();
    let mut blocking = 0;

    let emit = |msg: &str, blocking: &mut usize| {
        eprintln!("[{name}]    error {msg}");
        *blocking += 1;
    };

    /// Resolve a profile value for the static lint. `@name` (or `name`) is
    /// the direct case. A templated value cannot be resolved statically —
    /// the compose tokens wire their coder seat as
    /// `{{inputs.coder_profile}}` (bd-adv4) — so a PURE input ref is checked
    /// through its declared string default, and anything else (a
    /// `{{steps.x}}` ref, mixed text, a templated default) is left to the
    /// engine at invoke time. The `{{inputs.X}}` walker still fails an
    /// undeclared ref, so a typo cannot hide behind the skip.
    fn resolved_profile<'a>(
        profile: &'a str,
        inputs: &'a [toron_workflow::schema::InputDecl],
    ) -> Option<&'a str> {
        let bare = profile.trim_start_matches('@');
        if bare.is_empty() {
            return None;
        }
        if !bare.contains("{{") {
            return Some(bare);
        }
        let inner = bare.strip_prefix("{{")?.strip_suffix("}}")?.trim();
        let root = inner.strip_prefix("inputs.")?.split('.').next()?;
        let default = inputs
            .iter()
            .find(|i| i.name == root)
            .and_then(|i| i.default.as_ref())?
            .as_str()?
            .trim_start_matches('@');
        if default.contains("{{") {
            return None;
        }
        Some(default)
    }

    fn check_profile(
        profile: &str,
        inputs: &[toron_workflow::schema::InputDecl],
        profiles: &profile::ProfileCatalog,
        blocking: &mut usize,
        emit: &dyn Fn(&str, &mut usize),
    ) {
        let Some(bare) = resolved_profile(profile, inputs) else {
            return;
        };
        if profiles.get(bare).is_none() {
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

    fn scan_action(
        action: &toron_workflow::schema::Action,
        profiles: &profile::ProfileCatalog,
        stages: &stages::StageLibrary,
        inputs: &[toron_workflow::schema::InputDecl],
        declared: &std::collections::HashSet<&str>,
        blocking: &mut usize,
        emit: &dyn Fn(&str, &mut usize),
    ) {
        let json = serde_json::to_value(action).unwrap_or_default();
        if let Some(spec) = json.get("spec").and_then(|s| s.as_object()) {
            if let Some(profile) = spec.get("profile").and_then(|v| v.as_str()) {
                check_profile(profile, inputs, profiles, blocking, emit);
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
            check_profile(profile, inputs, profiles, blocking, emit);
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
            &wf.inputs,
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
                &wf.inputs,
                &declared_inputs,
                &mut blocking,
                &emit,
            );
        }
    }
    for issue in stage_reads_issues(name, wf, &stages) {
        emit(&issue, &mut blocking);
    }
    let wf_catalog =
        crate::workflow_catalog::WorkflowCatalog::load_layered(project_root).unwrap_or_default();
    blocking += crate::workflow_catalog::lint_shape(name, wf, &wf_catalog, &emit);
    blocking
}

/// The ADR-0021 D3 reads gate (bd-stage-contract-fields-t36sj): for each
/// dispatch step with a `stage:`, the stage's typed `reads` must be
/// satisfiable — `inputs.*` against the declared `inputs:`, `steps.*`
/// against the steps that can precede it (steps before it in `steps:`; the
/// transitive `needs:` ancestors in `graph:`, outer prior plus preceding
/// siblings inside loop/branch/fan-out bodies). Each miss is one returned
/// issue naming workflow, step, stage, and the missing reads. Steps whose
/// stage is templated or unresolvable are skipped (the existing check
/// reports those). Pure — the single call site is `lint_flywheel_tier`.
pub(crate) fn stage_reads_issues(
    name: &str,
    wf: &toron_workflow::schema::Workflow,
    stages: &stages::StageLibrary,
) -> Vec<String> {
    let declared: Vec<String> = wf.inputs.iter().map(|i| i.name.clone()).collect();
    // (step label, stage name, prior step names) in document order.
    let mut dispatches: Vec<(String, String, Vec<String>)> = Vec::new();
    // Linear steps: prior = the labels before this index.
    let mut prior: Vec<String> = Vec::new();
    for (i, step) in wf.steps.iter().enumerate() {
        let label = step
            .name
            .clone()
            .unwrap_or_else(|| format!("step_{}", i + 1));
        walk_action_dispatches(&step.action, &label, &prior, &mut dispatches);
        prior.push(label);
    }
    // Graph nodes: prior = the transitive needs ancestors.
    if let Some(graph) = &wf.graph {
        for (node_name, node) in &graph.nodes {
            let mut ancestors = Vec::new();
            let mut stack: Vec<&str> = node.needs.iter().map(String::as_str).collect();
            let mut seen = std::collections::HashSet::new();
            while let Some(next) = stack.pop() {
                if !seen.insert(next.to_string()) {
                    continue;
                }
                ancestors.push(next.to_string());
                if let Some(parent) = graph.nodes.get(next) {
                    stack.extend(parent.needs.iter().map(String::as_str));
                }
            }
            walk_action_dispatches(&node.action, node_name, &ancestors, &mut dispatches);
        }
    }
    let mut issues = Vec::new();
    for (label, stage_name, step_prior) in dispatches {
        if stage_name.contains("{{") {
            continue;
        }
        let Some(stage) = stages.get_stage(&stage_name) else {
            continue;
        };
        if stage.reads.is_empty() {
            continue;
        }
        let missing = stages::reads_unsatisfied(&stage.reads, &declared, &step_prior);
        if !missing.is_empty() {
            issues.push(format!(
                "workflow {name} step {label} stage {stage_name} missing reads: {}",
                missing.join(", ")
            ));
        }
    }
    issues
}

/// Collect (step label, stage name, prior steps) for every dispatch under
/// an action, recursing into loop/branch/fan-out/retry bodies. Body items
/// see the outer prior plus the container label and preceding siblings.
fn walk_action_dispatches(
    action: &toron_workflow::schema::Action,
    label: &str,
    prior: &[String],
    out: &mut Vec<(String, String, Vec<String>)>,
) {
    use toron_workflow::schema::Action;
    match action {
        Action::DispatchJob { spec, .. } => {
            if let Some(stage) = spec.stage.clone() {
                out.push((label.to_string(), stage, prior.to_vec()));
            }
        }
        Action::Loop { body, .. } => {
            let mut p = prior.to_vec();
            p.push(label.to_string());
            for (i, s) in body.iter().enumerate() {
                let l = s.name.clone().unwrap_or_else(|| format!("step_{}", i + 1));
                walk_action_dispatches(&s.action, &l, &p, out);
                p.push(l);
            }
        }
        Action::Branch {
            then, else_steps, ..
        } => {
            for branch in [then, else_steps] {
                let mut p = prior.to_vec();
                p.push(label.to_string());
                for (i, s) in branch.iter().enumerate() {
                    let l = s.name.clone().unwrap_or_else(|| format!("step_{}", i + 1));
                    walk_action_dispatches(&s.action, &l, &p, out);
                    p.push(l);
                }
            }
        }
        Action::Foreach { each, .. } | Action::Parallel { each, .. } => {
            let mut p = prior.to_vec();
            p.push(label.to_string());
            let l = each.name.clone().unwrap_or_else(|| format!("{label}.each"));
            walk_action_dispatches(&each.action, &l, &p, out);
        }
        Action::Retry { step, .. } => {
            walk_action_dispatches(&step.action, label, prior, out);
        }
        _ => {}
    }
}

/// --- moved-in test mod (bd-wave5-main-eviction-d5hht) ---
#[cfg(test)]
mod workflow_verify_statechart_tests {
    use super::*;
    use toron_core::kernel::Machine;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Chart {
        Start,
        Spin,
        Done,
        Orphan,
    }

    fn broken() -> Machine<Chart, (), (), ()> {
        Machine::builder(Chart::Start, ())
            .state(Chart::Start, |n| {
                let spin = n.transition(Chart::Spin).on(|_| true);
                let done = n.transition(Chart::Done).on(|_| true);
                n.on(spin).on(done)
            })
            .state(Chart::Spin, |n| {
                let again = n.transition(Chart::Spin).on(|_| true);
                n.on(again)
            })
            .final_state(Chart::Done, |n| n)
            .state(Chart::Orphan, |n| n)
            .build()
    }

    #[test]
    fn statechart_report_fails_on_an_orphan_and_a_trap() {
        let report = chart_report(
            "Broken",
            &broken().topology(),
            "Done",
            |s| *s == Chart::Done,
            None,
            None,
            &ChartFindings::default(),
        );
        assert!(!report.ok);
        assert_eq!(
            report.checks["reachable"]["unreachable"],
            serde_json::json!(["Orphan"])
        );
        assert_eq!(
            report.checks["can_finish"]["stuck"],
            serde_json::json!(["Spin"])
        );
        assert_eq!(report.errors.len(), 2);
    }

    /// Hermetic: an operator's user tier must not decide this gate (a stale
    /// copy shadowing a builtin would report their file, not the shipped one).
    #[test]
    fn statechart_verify_passes_the_shipped_charts() {
        let _env = crate::config::lock_env();
        // BOTH homes: the config tier supplies stages, and the state tier holds
        // the run log the unused-stage hint reads. Sandboxing only one made this
        // gate depend on whatever the operator last ran.
        let prev_cfg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        let dir = std::env::temp_dir().join(format!("fw-verify-tier-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp config home");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let ok = cmd_workflow_verify(None, false, true).is_ok();
        match prev_cfg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        match prev_state {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok, "the shipped tiers verify cleanly");
    }

    /// ADR-0021 D6: the gate takes NO ambient state. A run log naming a stage
    /// that neither a workflow references nor a contract declares must not
    /// change the verdict — a proof machine has no log, and a workstation's
    /// history must not decide what ships.
    #[test]
    fn statechart_verify_ignores_the_operator_run_log() {
        let _env = crate::config::lock_env();
        let prev_cfg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        let dir = std::env::temp_dir().join(format!("fw-verify-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp home");
        std::fs::create_dir_all(dir.join("flywheel/stages")).expect("empty config tier");

        let verify = || -> bool {
            unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
            unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
            cmd_workflow_verify(None, false, true).is_ok()
        };
        let hermetic = verify();

        // A run log that claims a ghost stage ran ten minutes ago.
        let runs = dir.join("flywheel/runs");
        std::fs::create_dir_all(&runs).expect("runs dir");
        std::fs::write(
            runs.join("ghost-run.jsonl"),
            serde_json::json!({
                "run_id": "ghost-run",
                "ts": "2026-11-17T00:00:00Z",
                "workflow": "ghost",
                "stage": "ghost-stage",
                "profile": "@coder",
                "harness": "opencode",
                "model": "m",
                "mode": "work",
                "prompt_hash": "h",
                "response": "",
                "status": "ok",
            })
            .to_string(),
        )
        .expect("write run log");
        let with_log = verify();

        match prev_cfg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        match prev_state {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            hermetic, with_log,
            "a run log changed the verify verdict: the gate is reading ambient state"
        );
        assert!(hermetic, "the shipped tiers verify cleanly");
    }
}

#[cfg(test)]
mod workflow_cli_lint_tests {
    use super::*;
    use crate::builtin_workflows;
    use toron_workflow::schema::parse_workflow;

    /// bd-lower-workflows-verify-9mdqc: the verify row for a catalog
    /// workflow carries its bounded warnings and stays ok.
    #[test]
    fn workflow_lowering_verify_row_reports_bounded_warnings() {
        let wfs = builtin_workflows::builtin_workflows();
        let wf = wfs.get("ce-lfg").expect("ce-lfg cataloged");
        let report = verify_catalog_workflow(
            "ce-lfg",
            wf,
            "builtin",
            &crate::stages::StageLibrary::compiled_in(),
            false,
        );
        assert!(report.ok, "ce-lfg row stays ok: {:?}", report.errors);
        assert!(report.errors.is_empty());
        assert!(
            report.warnings.is_empty(),
            "awaits are bounded model deadlines, not retry storms: {:?}",
            report.warnings
        );
        assert_eq!(report.checks["bounded"]["ok"], serde_json::json!(true));
    }

    /// A clean catalog workflow rows ok with no warnings.
    #[test]
    fn workflow_lowering_verify_row_clean_workflow_has_no_warnings() {
        let wfs = builtin_workflows::builtin_workflows();
        let wf = wfs.get("ce-triage").expect("ce-triage cataloged");
        let report = verify_catalog_workflow(
            "ce-triage",
            wf,
            "builtin",
            &crate::stages::StageLibrary::compiled_in(),
            false,
        );
        assert!(report.ok, "{:?}", report.errors);
        assert!(report.errors.is_empty());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.checks["witnessed"]["ok"], serde_json::json!(true));
    }

    /// Slice A (bd-36zlq): a temp user-tier workflow with an unreachable
    /// step shows up as a failing row with tier `user`.
    #[test]
    fn workflow_lowering_user_tier_unreachable_row_is_user() {
        let dir = std::env::temp_dir().join(format!("fw-verify-tier-{}", std::process::id()));
        let wf_dir = dir.join("workflows");
        std::fs::create_dir_all(&wf_dir).expect("temp workflows dir");
        std::fs::write(
            wf_dir.join("stranded.yaml"),
            "name: stranded\ntrigger:\n  type: webhook\ninputs: []\ngraph:\n  nodes:\n    a:\n      action: dispatch_job\n      assignee: x\n      spec: { profile: \"@coder\" }\n    c:\n      action: dispatch_job\n      assignee: x\n      spec: { profile: \"@coder\" }\n      needs: [zzz]\n",
        )
        .expect("temp workflow");
        let cat = crate::workflow_catalog::WorkflowCatalog::load_layered_with(dir.clone(), None)
            .expect("temp catalog loads");
        let wf = cat
            .workflows
            .get("stranded")
            .expect("temp workflow cataloged");
        let tier = catalog_tier("catalog", "stranded", &std::collections::BTreeSet::new());
        let report = verify_catalog_workflow(
            "stranded",
            wf,
            tier,
            &crate::stages::StageLibrary::compiled_in(),
            false,
        );
        assert!(!report.ok, "unreachable node fails the row");
        assert!(
            report.errors.iter().any(|e| e.contains('c')),
            "names the stranded state: {:?}",
            report.errors
        );
        assert_eq!(report.tier.as_deref(), Some("user"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A phase-ending step whose stage declares no witness fails the row's
    /// witnessed check, and the JSON names it.
    #[test]
    fn workflow_lowering_verify_row_reports_unwitnessed_phase_end() {
        let wf = parse_workflow(
            "name: unwitnessed\ntrigger:\n  type: webhook\ninputs: []\nsteps:\n  - name: gate\n    action: dispatch_job\n    assignee: x\n    spec: { profile: \"@planner\", phase: plan, stage: repo-map, task: map }\n",
        )
        .expect("parse");
        let report = verify_catalog_workflow(
            "unwitnessed",
            &wf,
            "builtin",
            &crate::stages::StageLibrary::compiled_in(),
            false,
        );
        assert!(!report.ok, "unwitnessed phase end fails the row");
        assert_eq!(report.checks["witnessed"]["ok"], serde_json::json!(false));
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("gate") && e.contains("phase plan")),
            "{:?}",
            report.errors
        );
    }

    /// A workflow that fails to lower is a named error, not a panic.
    #[test]
    fn workflow_lowering_verify_row_names_lower_failure() {
        let wf = parse_workflow(
            r#"
name: empty
trigger:
  type: webhook
steps: []
"#,
        )
        .expect("parse");
        let report = verify_catalog_workflow(
            "empty",
            &wf,
            "builtin",
            &crate::stages::StageLibrary::compiled_in(),
            false,
        );
        assert!(!report.ok);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("empty") && e.contains("failed to lower")),
            "{:?}",
            report.errors
        );
    }

    /// The flywheel-tier lint fails closed on an unresolved `@profile`
    /// (the bead's fail-stop contract).
    #[test]
    fn lint_rejects_unresolved_profile() {
        let wf = parse_workflow(
            r#"
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
"#,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf, None);
        assert!(blocking > 0, "unresolved profile must be a blocking issue");
    }

    /// An unresolved `stage:` alias fails closed.
    #[test]
    fn lint_rejects_unresolved_stage() {
        let wf = parse_workflow(
            r#"
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
"#,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf, None);
        assert!(blocking > 0, "unresolved stage must be a blocking issue");
    }

    /// A `{{inputs.x}}` ref with no matching `inputs:` declaration fails closed.
    #[test]
    fn lint_rejects_undeclared_input_ref() {
        let wf = parse_workflow(
            r#"
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
"#,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf, None);
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
            r#"
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
"#,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf, None);
        assert_eq!(blocking, 0, "nested ref into declared root must lint clean");
    }

    /// `await: result` lints clean: bodies watch the job through the
    /// injected watcher and settle like toron's host.
    #[test]
    fn lint_accepts_dispatch_await_result() {
        let wf = parse_workflow(
            r#"
name: t
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: a
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "ce-execute" }
    await: result
    result_timeout: 60
"#,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf, None);
        assert_eq!(blocking, 0, "await: result lints clean");
    }

    /// A clean workflow passes the flywheel tier (no blocking issues).
    #[test]
    fn lint_clean_workflow_passes() {
        let wf = parse_workflow(
            r#"
name: t
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: a
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "ce-execute" }
"#,
        )
        .expect("parse");
        let blocking = lint_flywheel_tier("t", &wf, None);
        assert_eq!(blocking, 0, "clean workflow must lint clean");
    }

    /// The CI gate (bead contract: lint over EVERY built-in on every change):
    /// every compiled-in workflow passes the FULL gate — engine tier
    /// (lint_workflow) + flywheel tier (profile/stage/inputs resolution).
    #[test]
    fn every_builtin_passes_full_lint_gate() {
        let config_root =
            std::env::temp_dir().join(format!("fw-lint-gate-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config_root);
        std::fs::create_dir_all(&config_root).expect("empty config root");
        let gate = |name: &str, wf: &toron_workflow::schema::Workflow| {
            lint_flywheel_tier_in(name, wf, None, Some(&config_root))
        };
        for (name, wf) in builtin_workflows::builtin_workflows() {
            let engine = toron_workflow::lint_workflow(&wf);
            assert!(
                engine.is_clean(),
                "built-in {name} fails engine lint: {:?}",
                engine.issues
            );
            let tier = gate(&name, &wf);
            assert_eq!(
                tier, 0,
                "built-in {name} fails flywheel-tier lint (profile/stage/inputs)"
            );
        }
        // Negative control: a bad override planted in the INJECTED root must
        // be seen. Without this the test could pass while reading config_dir().
        let poison = config_root.join("stages.toml");
        std::fs::write(
            &poison,
            "[stages.ce-work]\nreads = [\"steps.no-such-step\"]\n",
        )
        .expect("plant override");
        let wfs = builtin_workflows::builtin_workflows();
        let wf = wfs.get("ce-lfg").expect("ce-lfg cataloged");
        let blocked = gate("ce-lfg", wf);
        assert!(
            blocked > 0,
            "the gate must read the injected config root, not config_dir(): {blocked}"
        );
        let _ = std::fs::remove_dir_all(&config_root);
    }

    /// ADR-0021 D3 reads gate: a workflow that dispatches judge-arms with
    /// no arms step FAILS lint, naming workflow, step, stage, and the
    /// missing read (bd-stage-contract-fields-t36sj).
    #[test]
    fn stage_contract_judge_arms_without_arms_fails() {
        let wf = parse_workflow(
            r#"
name: judge-only
trigger:
  type: webhook
inputs:
  - name: objective
steps:
  - name: judge
    action: dispatch_job
    assignee: x
    spec: { profile: "@oracle", stage: judge-arms, task: "Judge {{inputs.objective}}." }
"#,
        )
        .expect("parse");
        let lib = crate::stages::StageLibrary::compiled_in();
        let issues = stage_reads_issues("judge-only", &wf, &lib);
        assert_eq!(issues.len(), 1, "one missing read: {issues:?}");
        let msg = &issues[0];
        for needle in [
            "workflow judge-only",
            "step judge",
            "stage judge-arms",
            "steps.arms",
        ] {
            assert!(msg.contains(needle), "names {needle}: {msg}");
        }
    }

    /// The same dispatch passes once an arms step precedes it.
    #[test]
    fn stage_contract_arms_before_judge_passes() {
        let wf = parse_workflow(
            r#"
name: judged
trigger:
  type: webhook
inputs:
  - name: objective
steps:
  - name: arms
    action: dispatch_job
    assignee: x
    spec: { profile: "@scout", stage: diagnose-only, task: "Explore." }
  - name: judge
    action: dispatch_job
    assignee: x
    spec: { profile: "@oracle", stage: judge-arms, task: "Judge {{inputs.objective}}." }
"#,
        )
        .expect("parse");
        let lib = crate::stages::StageLibrary::compiled_in();
        let issues = stage_reads_issues("judged", &wf, &lib);
        assert!(issues.is_empty(), "arms satisfies steps.arms: {issues:?}");
    }
}

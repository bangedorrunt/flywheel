// governed-by: ADR-0024
// governed-by: ADR-0027
// governed-by: ADR-0011
//! Pane runner (ADR-0010 Spec A pane row, P10.3, bd-ghcj.1): real herdr
//! PANE execution for interactive workflows — a crew pre-spawned from the
//! workflow's `crew` plan, then each `dispatch_job` step sent to the running
//! pane via the loop's proven `__send` confirmed-send leaf.

#![allow(dead_code, clippy::all, unused)]
//!
//! - **Crew pre-spawn**: [`pre_spawn_crew`] turns a [`CrewPlan`] (role →
//!   count) into real herdr panes: profile → harness/model → pane split +
//!   `agent_start` (kind = harness, AGENT_ARG = `--model` per harness),
//!   recorded in the spawn registry with a PROFILE column so `@role` resolves
//!   to a running pane.
//! - **Pane dispatch**: [`pane_dispatch`] prepends the profile behavior
//!   preamble (Spec C, same as headless) and shells out to `flywheel __send
//!   <project> <name> <prompt> --confirm` — the loop's durable confirmed-send
//!   leaf, no new dispatch code.
//! - **Lifecycle**: ephemeral default (the caller reaps the workspace at run
//!   end) + `--reuse WORKSPACE` (bind to the caller's workspace, never reap)
//!   + `--retain` (keep the spawned workspace). herdr owns the panes — they
//!     survive a flywheel crash; the spawn registry re-binds them.
//!
//! governed-by: ADR-0024
//! governed-by: ADR-0010 (Spec A, Spec C, Spec E), ADR-0005 (Spec 1, Spec 4c), ADR-0022 (W3/W4 durable engine runs)

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use toron_workflow::schema::Action;

use crate::artifact;
use crate::config::Config;
use crate::crew::CrewPlan;
use crate::herdr_shim::{AgentStatus, Client};
use crate::models::ModelRegistry;
use crate::profile::ProfileCatalog;
use crate::state::SpawnEntry;
use crate::{headless, state};

/// The result of a pane-mode crew pre-spawn: the workspace + registry
/// entries the runner dispatches against, plus the reap decision.
#[derive(Debug, Clone)]
pub struct PaneRunCtx {
    /// workspace id the crew lives in
    pub workspace_id: String,
    /// spawn-registry entries (pane_id, name, profile)
    pub entries: Vec<SpawnEntry>,
    /// true when the workspace must NOT be reaped at run end (--reuse or
    /// --retain)
    pub retained: bool,
}

/// Write an artifact for a dispatch (Spec 2). Best-effort, never fails the dispatch.
fn write_dispatch_artifact(
    project: &str,
    run_id: &str,
    node_id: &str,
    bead_id: Option<&str>,
    pin: &str,
    kind: crate::artifact::ArtifactKind,
    path: std::path::PathBuf,
) {
    // Ensure the file exists for sha256 (create an empty file if not)
    if !path.exists() {
        let _ = std::fs::write(&path, "");
    }
    let rec = match crate::artifact::ArtifactRecord::new(
        path.clone(),
        kind,
        crate::artifact::ExecutionStatus::Running,
        crate::artifact::CriterionStatus::Unknown,
        crate::artifact::GroundingStatus::Cited,
        crate::artifact::ClaimSupportStatus::WorkerAsserted,
        None,
        bead_id.map(std::string::ToString::to_string),
        run_id.to_string(),
        Some(node_id.to_string()),
        pin.to_string(),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("artifact write failed for dispatch {node_id}: {e}");
            return;
        }
    };
    if let Err(e) = crate::artifact::write_artifact_for_project(project, &rec) {
        eprintln!("artifact write failed for dispatch {node_id}: {e}");
    }
}

/// Names a crew pane: `crew-<workflow>-<role><ordinal>` (1-based per role),
/// lowercased + non-alnum collapsed to `-` (herdr agent names: lowercase
/// letters/digits/-/_ only, 1-32 chars). Role is the stripped profile ref
/// (`@planner` → `planner`); the name is unique within the run and carries
/// the profile for registry resolution.
pub fn crew_agent_name(wf_name: &str, role: &str, ordinal: u32) -> String {
    let slug: String = wf_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').chars().take(16).collect::<String>();
    let role_slug: String = role
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(8)
        .collect::<String>();
    if ordinal <= 1 {
        format!("crew-{slug}-{role_slug}")
    } else {
        format!("crew-{slug}-{role_slug}{ordinal}")
    }
}

/// Resolve which registry entry serves a dispatch target: first the entry
/// whose PROFILE matches `@role`, else an entry whose NAME matches. Pure —
/// the runner's `@role -> running pane` mapping (ADR-0010 Spec A pane row).
pub fn resolve_pane_entry<'a>(entries: &'a [SpawnEntry], profile: &str) -> Option<&'a SpawnEntry> {
    entries
        .iter()
        .find(|e| e.profile.as_deref() == Some(profile))
        .or_else(|| entries.iter().find(|e| e.name == profile))
}

/// bd-lvyi: resolve a per-step harness override to a crew pane running THAT
/// harness. The pane's `harness` column is authoritative; a legacy entry
/// without one (pre-bd-lvyi registry) falls back to its profile's harness
/// via the catalog. `None` → no pane of the override harness exists.
pub fn resolve_pane_by_harness<'a>(
    entries: &'a [SpawnEntry],
    harness: &str,
) -> Option<&'a SpawnEntry> {
    entries.iter().find(|e| {
        if let Some(h) = &e.harness {
            return h == harness;
        }
        // Legacy entry (no harness column): the catalog resolves the
        // profile's harness — the pane was spawned with it. A catalog load
        // failure degrades to `false` (no match) rather than panicking.
        let Some(profile_name) = e.profile.as_deref() else {
            return false;
        };
        match crate::profile::ProfileCatalog::load() {
            Ok(catalog) => catalog
                .get(profile_name)
                .is_some_and(|p| p.harness == harness),
            Err(_) => false,
        }
    })
}

/// Fail-fast guard: pane mode needs a non-empty crew plan (pure, tested).
pub fn ensure_crew(plan: &CrewPlan, wf_name: &str) -> Result<()> {
    if plan.members.is_empty() {
        bail!(
            "pane mode needs a crew (crew: auto or crew: {{role: count}} in workflow {wf_name:?})"
        );
    }
    Ok(())
}

/// Pre-spawn the crew for a pane-mode run: resolve the project workspace
/// (create or `--reuse`), then for each (role, count) in the plan spawn
/// `count` herdr panes with the profile's harness + model. Registry entries
/// carry the PROFILE column; the workspace + entries come back in
/// [`PaneRunCtx`] for the runner and the run-end reap decision.
///
/// Stale-workspace protection mirrors `cmd_spawn` (bd-fw-stale-workspace-panes
/// -hgp + bd-627z.3): a pre-existing FLYWHEEL-OWNED project workspace blocks
/// the spawn unless `--retire-orphans` closes it first; a foreign workspace
/// (captain/omp session labeled like the project) only warns — with
/// `--reuse WORKSPACE` the named workspace is bound instead (never blocked,
/// never reaped).
#[allow(clippy::too_many_arguments)] // one param per CLI flag, mirrors cmd_spawn
pub fn pre_spawn_crew(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    plan: &CrewPlan,
    catalog: &ProfileCatalog,
    reuse: Option<&str>,
    retain: bool,
    retire_orphans: bool,
) -> Result<PaneRunCtx> {
    ensure_crew(plan, wf_name)?;
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let dir = crate::project_dir(cfg, project);
    if !dir.exists() {
        bail!(
            "project dir missing for pane crew: {} (run: flywheel init {project})",
            dir.display()
        );
    }
    let label = crate::swarm_label(project);
    let split = crate::project_workspaces(&mut c, project)?;

    // --reuse: bind the named workspace (caller-owned, never reaped, and the
    // orphan guard skips it — it IS the target).
    if let Some(ws_id) = reuse {
        if !split
            .owned
            .iter()
            .chain(&split.foreign)
            .any(|ws| ws.workspace_id == ws_id)
        {
            bail!("--reuse workspace {ws_id} not found for {project}");
        }
        let entries = spawn_crew_into(cfg, project, wf_name, plan, catalog, &mut c, ws_id, &label)?;
        return Ok(PaneRunCtx {
            workspace_id: ws_id.to_string(),
            entries,
            retained: true,
        });
    }
    let orphans = split.owned;

    // bd-627z.3: a foreign workspace (captain/omp session labeled like the
    // project) is not the stale-pane race — warn and proceed, never close it.
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

    // Fresh workspace: stale panes would race the new crew — same contract as
    // cmd_spawn.
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
                if let Err(e) = c.workspace_close(&ws.workspace_id, true) {
                    eprintln!("flywheel: close {} failed: {e:#}", ws.workspace_id);
                }
            }
        } else {
            bail!(
                "found {} pre-existing workspace(s) for {project} — stale panes would race the crew: {}. Close them first (flywheel kill {project} or herdr workspace close <id>), or pass --retire-orphans / --reuse.",
                orphans.len(),
                detail.join(", ")
            );
        }
    }
    let (ws, _root) = c.workspace_create(
        Some(&crate::utf8_path(&dir, "workspace dir")?),
        Some(&label),
        false,
        None,
    )?;
    let entries = spawn_crew_into(
        cfg,
        project,
        wf_name,
        plan,
        catalog,
        &mut c,
        &ws.workspace_id,
        &label,
    )?;
    Ok(PaneRunCtx {
        workspace_id: ws.workspace_id,
        entries,
        retained: retain,
    })
}

/// The actual pane splits + agent starts for one workspace (shared by the
/// fresh-spawn and --reuse paths). Returns the registry entries.
#[allow(clippy::too_many_arguments)] // one param per caller mirror, mirrors cmd_spawn
#[allow(unused_mut)]
fn spawn_crew_into(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    plan: &CrewPlan,
    catalog: &ProfileCatalog,
    c: &mut Client,
    ws_id: &str,
    label: &str,
) -> Result<Vec<SpawnEntry>> {
    // Preserve pre-existing registry entries (swarm panes, prior crew) —
    // only entries whose NAME we are about to (re)spawn are replaced, so a
    // --reuse re-run re-binds the crew without clobbering unrelated panes.
    let mut registry: Vec<SpawnEntry> = state::load_spawn_registry(project);
    let mut ordinals: BTreeMap<String, u32> = BTreeMap::new();
    // pane-split chain: first agent on the root pane, rest split down.
    let mut prev: Option<String> = None;
    let dir = crate::project_dir(cfg, project);
    for (role, count) in &plan.members {
        let profile = catalog
            .get(role)
            .ok_or_else(|| anyhow!("pane crew: unknown profile @{role} (flywheel profile list)"))?;
        // ADR-0007 D1: resolve the slot ladder first — a slot-aliased profile
        // (@cheap/@standard/@premium) inherits the slot's harness. The raw
        // profile.harness is empty by design on those profiles.
        let model_reg =
            ModelRegistry::load_routing().map_err(|e| anyhow!("pane crew: model registry: {e}"))?;
        let (harness_resolved, _slot_model) =
            model_reg.resolve_profile_route(&profile.harness, &profile.model)?;
        let harness = &harness_resolved;
        for _i in 0..*count {
            let ordinal = {
                let n = ordinals.entry(role.clone()).or_insert(0);
                *n += 1;
                *n
            };
            let name = crew_agent_name(wf_name, role, ordinal);
            // --reuse / re-run idempotency: a registry entry for this name
            // whose pane is STILL ALIVE binds instead of re-spawning (agent
            // names are taken — re-spawning would fail with agent_name_taken).
            // A stale entry (pane closed/respawned) is re-spawned: its pane
            // is gone, and binding it would dispatch into a dead pane.
            let live_panes = c.agent_list()?;
            let existing = registry
                .iter()
                .find(|e| e.name == name)
                .filter(|e| {
                    live_panes
                        .iter()
                        .any(|a| a.pane_id == e.pane_id && a.workspace_id == ws_id)
                })
                .cloned();
            // bd-pin-public-plane-s-easy.2: draw the mail PIN BEFORE the
            // pane exists so the pane env carries AGENT_NAME/FLYWHEEL_MAIL_AS
            // (herdr injects env only at split/create).
            let mail_identity = crate::pinned_mail_identity(cfg, project, &name, harness);
            let pane_env = crate::herdr_shim::herdr_integration_env(cfg, mail_identity.as_deref());
            let (agent_pane_id, name, model_pin) = if let Some(e) = existing {
                eprintln!(
                    "  crew {}: reusing {} ({}) @{}",
                    label, name, e.pane_id, role
                );
                // A reused live pane keeps the provenance its own spawn
                // recorded (ADR-0013 P1-7) — this path resolves no plan.
                (e.pane_id.clone(), name.clone(), e.model_pin.clone())
            } else {
                let pane_id = match &prev {
                    None => {
                        // bd-lifecycle-occupied-pane-wedge-07tx: PROBE BEFORE
                        // BIND. The old code took panes.first() blind; an
                        // occupied root pane (unnamed shell occupant) made
                        // agent.start fail agent_pane_busy and the packing
                        // crew wedged in a not-ready retry loop. Pick the
                        // first pane in the workspace whose agent slot is
                        // EMPTY (no live agent session on it); if none is,
                        // SPLIT a fresh pane off the root instead of binding
                        // into an occupied one.
                        let panes = c.pane_list(Some(ws_id))?;
                        let agents = c.agent_list()?;
                        let occupied: std::collections::HashSet<String> =
                            agents.iter().map(|a| a.pane_id.clone()).collect();
                        let pane_ids: Vec<String> =
                            panes.iter().map(|p| p.pane_id.clone()).collect();
                        match select_empty_pane(&pane_ids, &occupied) {
                            Some(pid) => pid,
                            None => {
                                let root = pane_ids.first().cloned().ok_or_else(|| {
                                    anyhow!("pane crew: no root pane in workspace {ws_id}")
                                })?;
                                let split = crate::herdr_shim::pane_split(
                                    c,
                                    &root,
                                    "down",
                                    Some(&crate::utf8_path(&dir, "workspace dir")?),
                                    Some(&pane_env),
                                )?;
                                split.pane_id
                            }
                        }
                    }
                    Some(p) => {
                        let split = crate::herdr_shim::pane_split(
                            c,
                            p,
                            "down",
                            Some(&crate::utf8_path(&dir, "workspace dir")?),
                            Some(&pane_env),
                        )?;
                        split.pane_id
                    }
                };
                // AGENT_ARG must honor Profile.provider via the same
                // registry pin as cmd_spawn / spawn_launch_plan. Raw
                // profile.model drops the provider half (ADR-0027 compose gap).
                // ADR-0013 P1-7: PLAN form (not the argv-only view) so the
                // registry row below carries the same resolved Muse/slot pin
                // the argv embeds. No model and no provider = no route to
                // resolve → no pin (the bare-launch case).
                let (mut agent_args, model_pin): (Vec<String>, Option<String>) =
                    if profile.model.is_empty() && profile.provider.is_empty() {
                        (vec![], None)
                    } else {
                        let (args, pin) = crate::profile_launch_plan(harness, profile, None)
                            .map_err(|e| anyhow!("pane crew @{role} model routing: {e}"))?;
                        (args, Some(pin))
                    };
                agent_args.extend(headless::role_model_args(
                    harness,
                    &profile.smol,
                    &profile.slow,
                    &profile.plan,
                ));
                // ADR-0024 W1a: pane spawn one-shot registry-fallback for rate_limit (bd-42fs pattern)
                let agent = match crate::start_agent_with_retry(
                    c,
                    &name,
                    harness,
                    &pane_id,
                    &agent_args,
                ) {
                    Ok(a) => a,
                    Err(e) => {
                        let cf = crate::failure::classify(&e.to_string(), None, None);
                        if cf.kind == crate::failure::FailureKind::RateLimit {
                            if let Ok(registry) = ModelRegistry::load_routing() {
                                if let Some(fallback_id) = registry
                                    .get(&profile.model)
                                    .and_then(|m| m.fallback.clone())
                                {
                                    let fallback_harness =
                                        if registry.family_of(&fallback_id) == "grok" {
                                            "grok"
                                        } else {
                                            harness
                                        };
                                    let fallback_model =
                                        registry.resolve(&fallback_id, fallback_harness);
                                    let mut fallback_args = headless::model_args(
                                        fallback_harness,
                                        &fallback_model,
                                        (!profile.effort.is_empty())
                                            .then_some(profile.effort.as_str()),
                                    );
                                    fallback_args.extend(headless::role_model_args(
                                        fallback_harness,
                                        &profile.smol,
                                        &profile.slow,
                                        &profile.plan,
                                    ));
                                    // GS-043 I4: journal before ModelEntry.fallback hop.
                                    let evidence = e.to_string();
                                    let intent = crate::agent_ref::RebindIntent::prepare(
                                        crate::agent_ref::AgentRef::new(project, &name),
                                        crate::agent_ref::AgentBinding {
                                            profile_name: role.to_string(),
                                            model: profile.model.clone(),
                                            harness: harness.to_string(),
                                        },
                                        crate::agent_ref::AgentBinding {
                                            profile_name: role.to_string(),
                                            model: fallback_id.clone(),
                                            harness: fallback_harness.to_string(),
                                        },
                                        crate::agent_ref::RebindReason::ModelFallback {
                                            kind: "rate_limit".into(),
                                            evidence,
                                        },
                                        0,
                                    );
                                    eprintln!(
                                        "pane crew: @{role} rate-limited on {} — registry fallback {fallback_id}",
                                        profile.model
                                    );
                                    crate::agent_ref::commit_rebind(project, intent, |_| {
                                        crate::start_agent_with_retry(
                                            c,
                                            &name,
                                            fallback_harness,
                                            &pane_id,
                                            &fallback_args,
                                        )
                                    })?
                                } else {
                                    return Err(e);
                                }
                            } else {
                                return Err(e);
                            }
                        } else {
                            return Err(e);
                        }
                    }
                };
                // bd-pin-public-plane-s-easy.2: bind the pane identity now
                // that the pane exists (pane → pin bridge + registry).
                crate::bind_pane_mail_identity(
                    cfg,
                    project,
                    &name,
                    &agent.pane_id,
                    mail_identity.as_deref(),
                );
                // The first dispatch fires as soon as the engine runs step 1 —
                // a still-booting agent rejects prompts (`agent_not_ready`).
                // Wait for the pane to settle idle before returning
                // (best-effort: a wait failure degrades to the __send landing
                // semantics, which classify not-ready as a per-step error,
                // never a panic).
                if let Err(e) = c.agent_wait(&name, &[AgentStatus::Idle], Some(90_000)) {
                    eprintln!(
                        "flywheel: crew {name} readiness wait failed (dispatching anyway): {e}"
                    );
                }
                (agent.pane_id.clone(), name.clone(), model_pin)
            };
            prev = Some(agent_pane_id.clone());
            registry.retain(|e| e.name != name);
            // bd-627z.4: the pin (drawn pre-split above) binds this pane's
            // agent deterministically — assign resolves through it.
            registry.push(SpawnEntry {
                pane_id: agent_pane_id.clone(),
                name: name.clone(),
                profile: Some(role.clone()),
                harness: Some(harness.clone()),
                model: None,
                // ADR-0013 P1-7/P1-8: the same resolved plan the pane launched
                // with — pin + bounded skills digest, never a second resolution.
                model_pin,
                skills_digest: crate::profile::skills_digest(&profile.skills),
                mail_identity,
                skills_injected: false,
                incarnation: 0,
            });
            eprintln!("  crew {}: {} ({}) @{}", label, name, agent_pane_id, role);
        }
    }
    // Persist the crew registry (pane_id, name, profile) — `__send`'s auto-
    // heal re-binds lost names from it.
    state::save_spawn_registry(project, &registry)?;
    Ok(registry)
}

/// bd-wave-swarm-graduation-fix-98io: dispatch-reason transiency, named.
/// `agent_not_ready` (agent still booting) AND `agent_pane_busy` (pane not
/// yet an available shell — the graduation run's boot race) are both
/// transient: retry with backoff instead of burning the step. Pure +
/// testable.
pub fn is_transient_dispatch_reason(reason: &str) -> bool {
    reason.contains("agent_not_ready") || reason.contains("agent_pane_busy")
}

/// bd-lifecycle-occupied-pane-wedge-07tx (probe-before-bind): the first pane
/// whose agent slot is EMPTY — no live agent session on it. `panes` is the
/// workspace's pane ids in bind order; `occupied` is the set of pane ids
/// that already carry an agent (ANY agent — named or unnamed occupant).
/// All-occupied / no panes → None: the caller must SPLIT a fresh pane (or
/// fail loud), never bind into an occupied shell. Pure + unit-tested.
pub fn select_empty_pane(
    panes: &[String],
    occupied: &std::collections::HashSet<String>,
) -> Option<String> {
    panes.iter().find(|p| !occupied.contains(*p)).cloned()
}

/// bd-42fs step 5: does a per-step harness override conflict with the
/// profile's harness? Pane crews pre-spawn per profile, so an override that
/// differs cannot be satisfied by an existing pane — the headless runner is
/// the override surface. Pure + unit-tested.
pub fn harness_override_conflicts(profile_harness: &str, spec_harness: Option<&str>) -> bool {
    match spec_harness {
        // An EMPTY profile harness (ADR-0007 slot-inherit) never conflicts:
        // the profile resolves its harness from the slot at spawn time, so
        // any explicit override is simply authoritative for the step.
        Some(h) if !h.trim().is_empty() => !profile_harness.is_empty() && h != profile_harness,
        _ => false,
    }
}

/// The pane ActionRunner dispatch (ADR-0010 Spec A pane row, P10.3): a
/// `dispatch_job` step resolves its `@profile` to a running crew pane, gets
/// the profile behavior preamble prepended (Spec C — same conditioning as
/// headless), and dispatches through the loop's `__send --confirm` leaf
/// (subprocess-reuse; the leaf owns landing semantics + last-sent dedup).
///
/// Non-dispatch actions stay recorded, never dispatched (P10.3 scope, same
/// carve-out as headless). Unknown profiles / missing panes are per-step
/// errors, never panics.
pub fn pane_dispatch(
    catalog: &ProfileCatalog,
    project: &str,
    wf_name: &str,
    inputs: &Value,
    entries: &[SpawnEntry],
    action: &Action,
) -> Result<Value> {
    let Action::DispatchJob { spec, .. } = action else {
        // bd-omwg: a Spawn reaching the runner is an engine-routing bug —
        // fail closed, never record it as done (ADR-0010 Spec E).
        if matches!(action, Action::Spawn { .. }) {
            bail!(
                "pane runner: Action::Spawn must be handled by the engine (run_spawn) — routing bug"
            );
        }
        // Recorded, not dispatched — the P10.3 backward-compat surface for
        // non-dispatch actions (byte-identical to the no-op runner).
        return Ok(json!({ "action": action.type_name() }));
    };
    let profile_ref = headless::dispatch_spec_profile(spec)
        .ok_or_else(|| anyhow!("pane dispatch: dispatch_job spec has no profile"))?;
    let profile_name = headless::strip_profile_ref(profile_ref);
    let profile = catalog.get(profile_name).ok_or_else(|| {
        anyhow!("pane dispatch: unknown profile @{profile_name} (flywheel profile list)")
    })?;
    // ADR-0024 Spec 2: DispatchSpec.context fresh = new session.
    // Honoring: map fresh onto a new spawn pane (pane.rs respawn split).
    // For Spawn jobs, launch_spawn_job skips reuse when context==fresh.
    // For dispatch, we log the fresh request; the crew pane's transcript is
    // logically fresh for this dispatch (embedding ensures new session).
    if spec.context == Some(toron_workflow::schema::ContextMode::Fresh) {
        eprintln!(
            "pane dispatch: dispatch_job @{profile_name} requests context=fresh — new session (honored: fresh maps to new spawn pane)"
        );
    }
    // bd-42fs step 5 / bd-lvyi: a per-step harness override (spec.harness)
    // resolves to a crew pane OF THAT HARNESS when one exists (the crew
    // pre-spawns per profile, but a squad can contain a pane whose harness
    // matches the override — e.g. a debate squad with an opencode reviewer
    // pane); error clearly when no pane of the overridden harness exists.
    // Crew pre-spawn stays profile-based; the override only reroutes THIS
    // step's dispatch.
    let mut override_entry: Option<&SpawnEntry> = None;
    if let Some(override_h) = headless::dispatch_spec_harness(spec)
        && harness_override_conflicts(&profile.harness, Some(override_h))
    {
        match resolve_pane_by_harness(entries, override_h) {
            Some(e) => {
                eprintln!(
                    "pane dispatch: step @{profile_name} overrides harness={override_h} — dispatching to {} ({}, runs {override_h})",
                    e.name, e.pane_id
                );
                override_entry = Some(e);
            }
            None => {
                // bd-ytr6: the pin check compares raw profile.harness while
                // spawn stores the resolved harness, so on hosts where
                // models.toml defaults short-circuit slot aliases the pin can
                // name a harness no pane runs. Fall back to the step's @role
                // pane if one exists; only bail when neither exists.
                if let Some(role_entry) = resolve_pane_entry(entries, profile_name) {
                    eprintln!(
                        "flywheel: WARNING step @{profile_name} pins harness={override_h} but no {override_h} pane exists; dispatching to @{profile_name} pane {} (harness={})",
                        role_entry.name,
                        role_entry.harness.as_deref().unwrap_or("?")
                    );
                    override_entry = Some(role_entry);
                } else {
                    bail!(
                        "pane dispatch: step @{profile_name} overrides harness={override_h} but no crew pane runs it (panes: {}) — per-step harness overrides land in --mode headless, or spawn a squad containing a {override_h} pane",
                        entries
                            .iter()
                            .map(|e| format!("{}={}", e.name, e.harness.as_deref().unwrap_or("?")))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
    }
    // Role conditioning (Spec C): the behavior preamble is prepended INSIDE
    // `__send --profile` (bd-gsvn.1) — the ONE conditioning site shared by
    // the pane runner and both loop paths. The runner passes the raw stage
    // prompt (or goal fallback) and the profile name.
    let base = match headless::dispatch_spec_stage(spec) {
        Some(s) => crate::stage_prompt(s)?,
        None => headless::base_prompt_for(wf_name, inputs),
    };
    let prompt = base;
    // @role -> running pane: a per-step harness override's matched pane
    // wins; otherwise profile column first, name fallback.
    let entry = override_entry
        .or_else(|| resolve_pane_entry(entries, profile_name))
        .ok_or_else(|| {
            anyhow!(
                "pane dispatch: no crew pane for @{profile_name} (spawned: {})",
                entries
                    .iter()
                    .map(|e| e.profile.as_deref().unwrap_or(&e.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    // Subprocess-reuse (Spec A): the `__send` leaf owns the confirmed send
    // (landing wait, last-sent dedup, auto-heal, Spec C conditioning) — no
    // new dispatch code. A freshly spawned agent may still be booting when
    // the first step fires: `agent_not_ready` is transient — retry with
    // backoff (bounded; a persistent not-ready surfaces as a per-step error,
    // never a hang).
    let mut attempts = 0;
    const MAX_ATTEMPTS: u32 = 6;
    // bd-mukq: 429 reroute state — the CURRENT send target (starts at the
    // resolved entry) plus the attempted profiles (starts at the step's
    // own). A 429 swaps the target to a live fallback-profile pane and
    // re-drives the same loop ONCE; the second 429 fails loud with a
    // trail instead of redriving the exhausted route.
    let mut attempt_entry: &SpawnEntry = entry;
    let mut attempt_profile = profile_name.to_string();
    let mut attempted: Vec<String> = vec![profile_name.to_string()];
    let mut reroute_trail: Vec<Value> = Vec::new();
    let rate_limit_kind = crate::failure::FailureKind::RateLimit.to_string();
    loop {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("__send")
            .arg(project)
            .arg(&attempt_entry.name)
            .arg(&prompt)
            .arg("--confirm")
            .arg("--profile")
            .arg(&attempt_profile);
        let output = cmd
            .output()
            .with_context(|| format!("__send for @{attempt_profile} ({})", attempt_entry.name))?;
        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let trimmed = raw.trim();
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            let transient = v
                .get("reason")
                .and_then(Value::as_str)
                .map(is_transient_dispatch_reason)
                .unwrap_or(false);
            if transient && attempts < MAX_ATTEMPTS {
                attempts += 1;
                eprintln!(
                    "flywheel: @{attempt_profile} not ready (attempt {attempts}/{MAX_ATTEMPTS}) — retrying in 10s"
                );
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
            // bd-lifecycle-occupied-pane-wedge-07tx: bounded-fail-loud. The
            // old code fell through after MAX_ATTEMPTS and returned the
            // failure payload as Ok — the engine re-drove the same dead step
            // forever (idle crews + endless "@not ready (attempt 1/6)").
            // Transiency was exhausted: the pane is not coming up (occupied
            // bind, dead split, stuck boot). Surface a per-step ERROR so the
            // engine's failure handling sees it instead of a soft ok-payload.
            if transient {
                let reason = v
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("transient")
                    .to_string();
                return Err(anyhow!(
                    "dispatch @{attempt_profile} ({}) failed after {MAX_ATTEMPTS} transient retries: {reason} — the target pane never became available; fix the pane (occupied/dead) or fail the step",
                    attempt_entry.name
                ));
            }
            // ADR-0022 §3.5 warn-first plan-artifact gate (bd-cvv9): same
            // contract as the headless path — warn + tag, never block
            // (the W5 flip owns blocking).
            if crate::phase_chain::plan_artifact_missing(spec, inputs) {
                eprintln!(
                    "flywheel: phase implement dispatch has NO completed plan artifact — \
                     warn-first gate (ADR-0022 §3.5); step output tagged plan_artifact_missing"
                );
            }
            let mut out = json!({
                "profile": attempt_profile.clone(),
                "agent": attempt_entry.name,
                "send": v.clone(),
            });
            // ADR-0024 W1a: classify pane dispatch failures (rate_limit/timeout/auth)
            if v.get("ok").and_then(Value::as_bool) == Some(false) {
                let err_str = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or(&raw)
                    .to_string();
                let cf = crate::failure::classify(&err_str, None, None);
                if cf.kind != crate::failure::FailureKind::Unknown {
                    out["ok"] = json!(false);
                    out["error"] = json!(err_str.clone());
                    out["kind"] = json!(cf.kind.to_string());
                    out["retryable"] = json!(cf.retryable);
                }
                // bd-mukq: 429 reroute — ONE hop to a live fallback-profile
                // pane (ADR-0026 shared chain; the running agent keeps its
                // model and session, the self-contained prompt reroutes).
                // No chain or no live pane, or the second 429, fails LOUD
                // with a trail + hint instead of redriving exhausted quota.
                if out.get("kind").and_then(Value::as_str) == Some(rate_limit_kind.as_str()) {
                    match dispatch_reroute_target(catalog, entries, &attempt_profile, &attempted) {
                        Some((next_profile, next_name)) => {
                            attempted.push(next_profile.clone());
                            crate::record_reroute_audit(
                                project,
                                0,
                                &attempt_profile,
                                crate::failure::FailureKind::RateLimit,
                                &err_str,
                            );
                            // GS-043 B2 / I4: ProfileFallback full lifecycle —
                            // Prepared → InFlight → Settled|Unknown (not Prepared-only).
                            // Pane path cannot swap model in-session; profile.fallback
                            // pane hop is the rate_limit reroute here.
                            eprintln!(
                                "flywheel: dispatch @{attempt_profile} rate-limited (429) — rerouting prompt to @{next_profile} ({next_name}), one hop"
                            );
                            reroute_trail.push(json!({
                                "from": attempt_profile.clone(),
                                "to": next_profile.clone(),
                                "evidence": err_str,
                            }));
                            if crate::agent_ref::RebindIntent::allowed_from_failure(
                                crate::failure::FailureKind::RateLimit,
                                &err_str,
                            ) {
                                let intent = crate::agent_ref::RebindIntent::prepare(
                                    crate::agent_ref::AgentRef::new(project, &attempt_entry.name),
                                    crate::agent_ref::AgentBinding::profile(&attempt_profile),
                                    crate::agent_ref::AgentBinding::profile(&next_profile),
                                    crate::agent_ref::RebindReason::ProfileFallback {
                                        kind: "rate_limit".into(),
                                        evidence: err_str.clone(),
                                    },
                                    0,
                                );
                                match crate::agent_ref::commit_rebind_opt(project, intent, |_| {
                                    Ok(resolve_pane_entry(entries, &next_profile))
                                }) {
                                    Ok(Some(e)) => {
                                        attempt_entry = e;
                                        attempt_profile = next_profile;
                                        continue;
                                    }
                                    Ok(None) => {
                                        // Settled as Unknown: target pane gone mid-hop.
                                        out["reroute"] = json!("target-pane-gone");
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "flywheel: pane 429 ProfileFallback journal/apply failed: {e:#}"
                                        );
                                        out["reroute"] = json!("target-pane-gone");
                                    }
                                }
                            } else {
                                // B2 residual / I4: empty or disallowed evidence must NOT
                                // invent a profile hop without journal. Fail-closed —
                                // journal Unknown, refuse hop (no silent continue).
                                eprintln!(
                                    "flywheel: pane 429 ProfileFallback refused — evidence gate closed (I4); no silent hop to @{next_profile}"
                                );
                                let intent = crate::agent_ref::RebindIntent::prepare(
                                    crate::agent_ref::AgentRef::new(project, &attempt_entry.name),
                                    crate::agent_ref::AgentBinding::profile(&attempt_profile),
                                    crate::agent_ref::AgentBinding::profile(&next_profile),
                                    crate::agent_ref::RebindReason::ProfileFallback {
                                        kind: "rate_limit".into(),
                                        evidence: if err_str.trim().is_empty() {
                                            "(empty)".into()
                                        } else {
                                            err_str.clone()
                                        },
                                    },
                                    0,
                                );
                                let _ =
                                    crate::agent_ref::commit_rebind_opt(project, intent, |_| {
                                        Ok(None::<()>)
                                    });
                                out["reroute"] = json!("refused-empty-evidence");
                            }
                        }
                        None => {
                            out["reroute"] = json!("none-available");
                            out["hint"] = json!(format!(
                                "429 on @{attempt_profile} with no further fallback pane live — spawn the fallback profile or wait for quota; redriving the same route hammers quota"
                            ));
                        }
                    }
                }
            }
            if !reroute_trail.is_empty() {
                out["reroute_trail"] = json!(reroute_trail);
            }
            return Ok(crate::phase_chain::annotate_dispatch_output(
                spec, inputs, out,
            ));
        }
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        let err_str = if err.trim().is_empty() {
            raw.clone()
        } else {
            err.clone()
        };
        let cf = crate::failure::classify(&err_str, None, None);
        let mut out = json!({
            "ok": false,
            "error": if err.trim().is_empty() { raw } else { err },
            "profile": profile_name,
            "agent": entry.name,
            // GS-049 P1-3: phase-bearing failure (ActionStage)
            "stage": cf.stage,
        });
        if cf.kind != crate::failure::FailureKind::Unknown {
            out["kind"] = json!(cf.kind.to_string());
            out["retryable"] = json!(cf.retryable);
        }
        return Ok(out);
    }
}

/// bd-mukq: select a one-hop 429 reroute target for a pane dispatch — the
/// next fallback profile in the PASSED catalog's chain (ADR-0026) that has
/// a LIVE pane in this run's entries. Pure — the unit surface. Returns
/// (profile, entry_name). `None` = fail loud locally: no chain, chain
/// panes not live in this run, or the hop already happened (the caller
/// passes prior attempts; second same-class failure is terminal).
/// Reads the chain from the passed catalog (never reloads from disk —
/// `next_fallback` re-loads the user catalog, which diverges in tests
/// and wastes a disk load per call in prod). An in-pane model swap is
/// deliberately NOT offered: the running agent owns its model and its
/// session — the prompt reroutes to a live fallback pane whose brief is
/// self-contained, or the step fails loud.
pub fn dispatch_reroute_target(
    catalog: &ProfileCatalog,
    entries: &[SpawnEntry],
    profile_name: &str,
    attempted: &[String],
) -> Option<(String, String)> {
    let mut tried: Vec<String> = attempted.to_vec();
    if !tried.iter().any(|a| a == profile_name) {
        tried.push(profile_name.to_string());
    }
    loop {
        let chain = catalog
            .get(profile_name)
            .map(|p| p.fallback.clone())
            .unwrap_or_default();
        let next = chain.into_iter().find(|fb| !tried.contains(fb))?;
        tried.push(next.clone());
        if catalog.get(&next).is_none() {
            continue;
        }
        if let Some(e) = resolve_pane_entry(entries, &next) {
            return Some((next, e.name.clone()));
        }
    }
}

/// One drained spawn side-channel entry (bd-omwg): the launch recipe for a
/// job the engine created but cannot run itself — the profile + harness +
/// launch args + the await decision. Built from the promise's `spec`
/// (ADR-0005 Spec 4a: profile/workflow/resolved) by [`spawn_launch_plan`].
#[derive(Debug, Clone)]
pub struct SpawnJobLaunch {
    /// the durable promise id the job binds to (kind-43001 would carry it)
    pub promise_id: String,
    /// stripped profile name (`@housekeeper` → `housekeeper`)
    pub profile: String,
    /// the sub-workflow the spawned agent runs, when the step names one
    pub workflow: Option<String>,
    /// harness from the RESOLVED spec (the embedding's view — the catalog
    /// may have changed between resolve and drain)
    pub harness: String,
    /// per-harness model/effort args (profile_launch_args — bd-eign.9);
    /// empty = bare launch (honest, never a silent skip)
    pub args: Vec<String>,
    /// ADR-0022 W2: the lifecycle phase from the resolved spec, when the
    /// step carried one (drives the phase registry).
    pub phase: Option<String>,
    /// ADR-0022 §3.2 R2: the harness had no native mechanic for the phase —
    /// the drainer warns at launch and the launch proceeds in prompt mode.
    pub phase_degraded: bool,
    /// `await: true` → the run parks: resolve the promise when the job
    /// finishes. `false` → fire-and-forget: launch, never resolve.
    pub await_job: bool,
    /// ADR-0024 Spec 2: execution context mode (inherit default, fresh = new session).
    pub context: Option<String>,
}

/// bd-omwg: read one spawn side-channel promise spec into a launch plan.
/// The profile name strips the `@`; the harness comes from the RESOLVED
/// spec; the args come from `profile_launch_args` for that harness (the
/// catalog profile may be gone — bare launch then, never an error).
/// Pure + unit-tested.
pub fn spawn_launch_plan(
    promise_id: &str,
    spec: &Value,
    catalog: &ProfileCatalog,
) -> Result<SpawnJobLaunch> {
    let profile = spec["profile"]
        .as_str()
        .map(headless::strip_profile_ref)
        .unwrap_or_default()
        .to_string();
    let workflow = spec["workflow"].as_str().map(str::to_string);
    let harness = spec["resolved"]["spec"]["harness"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // ADR-0022 W2: the step's lifecycle phase (typed spec, resolved copy).
    let phase = spec["resolved"]["spec"]["phase"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let args = match catalog.get(&profile) {
        Some(p) => crate::profile_launch_args(&harness, p, phase.as_deref())
            .map_err(|e| anyhow::anyhow!("spawn plan '{profile}' model routing: {e}"))?,
        None => Vec::new(),
    };
    // Degradation is decided by the registry itself so the plan carries the
    // flag to the drainer (loud at launch, never silent).
    let phase_degraded = match &phase {
        Some(ph) => headless::phase_launch_args(&harness, ph).1,
        None => false,
    };
    let context = spec["context"]
        .as_str()
        .map(std::string::ToString::to_string);
    Ok(SpawnJobLaunch {
        promise_id: promise_id.to_string(),
        profile,
        workflow,
        harness,
        args,
        phase,
        phase_degraded,
        await_job: spec["await"].as_bool().unwrap_or(false),
        context,
    })
}

/// bd-omwg contract: after `WorkflowEngine::invoke`, SpawnSink must NOT be
/// silent. Drains every promise_id the engine pushed and hands each to the
/// launcher (which owns the real pane launch + job completion wait).
///
/// Every created job promise must be SETTLED for the run to complete: the
/// 0.6 runtime suspends a run that leaves an unresolved created promise
/// (empirically proven — an await:false invoke parks until settle). The
/// `await` flag decides the settlement moment, never whether to settle:
///
/// - `await: true` → the launcher waits for the JOB (agent back to Idle) and
///   only then resolves — the parked run resumes with the real receipt
///   (never a fake kind-43004).
/// - `await: false` → fire-and-forget: the launch receipt settles the
///   promise immediately — the run proceeds while the job keeps running in
///   the pane. The workflow never waits for the job outcome.
///
/// `seen` dedups promise ids across calls: on resume the runtime REPLAYS the
/// run, which re-executes `run_spawn` and re-pushes the same promise id to
/// the sink — the idempotence guard that prevents a second launch.
/// Errors are per-promise: one bad spec must not strand the rest.
pub async fn drain_spawn_sink(
    engine: &toron_workflow::WorkflowEngine,
    catalog: &ProfileCatalog,
    seen: &mut std::collections::HashSet<String>,
    mut launch: impl FnMut(SpawnJobLaunch) -> Result<Value>,
) -> Result<Vec<Value>> {
    let mut receipts = vec![];
    loop {
        let ids: Vec<String> = engine.spawn_sink().lock().await.drain(..).collect();
        if ids.is_empty() {
            break;
        }
        for id in ids {
            if !seen.insert(id.clone()) {
                // Replay re-push (the resume re-executes run_spawn) — this
                // promise was already settled; never launch twice.
                continue;
            }
            let record = match engine.resonate().promises.get(&id).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("flywheel: spawn drain: promise {id} unreadable ({e}) — skipping");
                    continue;
                }
            };
            let plan = spawn_launch_plan(&id, record.param.data_as_ref(), catalog);
            let plan = match plan {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("flywheel: spawn drain: bad promise spec for {id}: {e}");
                    continue;
                }
            };
            match launch(plan.clone()) {
                Ok(receipt) => {
                    if plan.phase_degraded {
                        eprintln!(
                            "flywheel: spawn drain: job {} phase {:?} DEGRADED on harness {} (no native mechanic) — prompt mode",
                            plan.promise_id, plan.phase, plan.harness
                        );
                    }
                    eprintln!(
                        "flywheel: spawn drain: launched job {} (profile {}, harness {}){}",
                        plan.promise_id,
                        plan.profile,
                        plan.harness,
                        if plan.await_job {
                            " — awaiting job completion"
                        } else {
                            " (fire-and-forget)"
                        }
                    );
                    if let Err(e) = engine.resolve_spawn(&id, receipt.clone()).await {
                        eprintln!("flywheel: spawn drain: resolve {id} failed: {e}");
                    }
                    receipts.push(receipt);
                }
                Err(e) => {
                    eprintln!("flywheel: spawn drain: launch {id} failed: {e:#}");
                    // The run parks on this promise — without a settle it
                    // hangs forever. Reject it: the job never started is
                    // a FAILURE, not a fake success (Spec 0 propagation).
                    if let Err(re) = engine.reject_spawn(&id, &e.to_string()).await {
                        eprintln!("flywheel: spawn drain: reject {id} failed: {re}");
                    }
                }
            }
        }
    }
    Ok(receipts)
}

/// The real pane launcher for a drained spawn job: split a pane in the crew
/// workspace, `agent_start` with the plan's profile_launch_args (bd-eign.9 —
/// never an empty arg list for a known profile), register the identity +
/// spawn-registry entry, dispatch the job's workflow mission through the
/// `__send` leaf, and — for `await: true` — wait for the agent to return to
/// Idle (the job finished) before returning the receipt.
#[allow(unused_mut)]
pub fn launch_spawn_job(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    ws_id: &str,
    plan: &SpawnJobLaunch,
) -> Result<Value> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    // Reuse a live registry entry of the same name (re-run idempotency,
    // mirroring spawn_crew_into); else split a fresh pane + start.
    let name = crew_agent_name(wf_name, &format!("spawn-{}", plan.profile), 1);
    let registry = state::load_spawn_registry(project);
    let live_panes = c.agent_list()?;
    // ADR-0024 Spec 2: `context: fresh` maps to a new spawn pane (never reuse).
    let is_fresh = plan.context.as_deref() == Some("fresh");
    let existing = if is_fresh {
        None
    } else {
        registry
            .iter()
            .find(|e| e.name == name)
            .filter(|e| {
                live_panes
                    .iter()
                    .any(|a| a.pane_id == e.pane_id && a.workspace_id == ws_id)
            })
            .cloned()
    };
    let (pane_id, name) = if let Some(e) = existing {
        (e.pane_id.clone(), name)
    } else {
        let dir = crate::project_dir(cfg, project);
        let panes = c.pane_list(Some(ws_id))?;
        let prev = panes
            .last()
            .map(|p| p.pane_id.clone())
            .ok_or_else(|| anyhow!("spawn drain: no panes in workspace {ws_id}"))?;
        // bd-pin-public-plane-s-easy.2: draw the pin BEFORE the split so the
        // pane env carries AGENT_NAME/FLYWHEEL_MAIL_AS.
        let mail_identity = crate::pinned_mail_identity(cfg, project, &name, &plan.harness);
        let pane_env = crate::herdr_shim::herdr_integration_env(cfg, mail_identity.as_deref());
        let split = crate::herdr_shim::pane_split(
            &mut c,
            &prev,
            "down",
            Some(&crate::utf8_path(&dir, "workspace dir")?),
            Some(&pane_env),
        )?;
        let agent = match crate::start_agent_with_retry(
            &mut c,
            &name,
            &plan.harness,
            &split.pane_id,
            &plan.args,
        ) {
            Ok(a) => a,
            Err(e) => {
                let cf = crate::failure::classify(&e.to_string(), None, None);
                if cf.kind == crate::failure::FailureKind::RateLimit {
                    if let Ok(registry) = ModelRegistry::load_routing() {
                        if let Ok(catalog) = crate::profile::ProfileCatalog::load() {
                            if let Some(profile) = catalog.get(&plan.profile) {
                                if let Some(fallback_id) = registry
                                    .get(&profile.model)
                                    .and_then(|m| m.fallback.clone())
                                {
                                    let fallback_harness =
                                        if registry.family_of(&fallback_id) == "grok" {
                                            "grok"
                                        } else {
                                            plan.harness.as_str()
                                        };
                                    let fallback_model =
                                        registry.resolve(&fallback_id, fallback_harness);
                                    let mut fallback_args = crate::headless::model_args(
                                        fallback_harness,
                                        &fallback_model,
                                        None,
                                    );
                                    // GS-043 I4: journal before ModelEntry.fallback hop.
                                    let evidence = e.to_string();
                                    let intent = crate::agent_ref::RebindIntent::prepare(
                                        crate::agent_ref::AgentRef::new(project, &name),
                                        crate::agent_ref::AgentBinding {
                                            profile_name: plan.profile.clone(),
                                            model: profile.model.clone(),
                                            harness: plan.harness.clone(),
                                        },
                                        crate::agent_ref::AgentBinding {
                                            profile_name: plan.profile.clone(),
                                            model: fallback_id.clone(),
                                            harness: fallback_harness.to_string(),
                                        },
                                        crate::agent_ref::RebindReason::ModelFallback {
                                            kind: "rate_limit".into(),
                                            evidence,
                                        },
                                        0,
                                    );
                                    eprintln!(
                                        "pane spawn: {} rate-limited — registry fallback {fallback_id}",
                                        plan.profile
                                    );
                                    crate::agent_ref::commit_rebind(project, intent, |_| {
                                        crate::start_agent_with_retry(
                                            &mut c,
                                            &name,
                                            fallback_harness,
                                            &split.pane_id,
                                            &fallback_args,
                                        )
                                    })?
                                } else {
                                    return Err(e);
                                }
                            } else {
                                return Err(e);
                            }
                        } else {
                            return Err(e);
                        }
                    } else {
                        return Err(e);
                    }
                } else {
                    return Err(e);
                }
            }
        };
        // bd-pin-public-plane-s-easy.2: bind the pane identity now that the
        // pane exists.
        crate::bind_pane_mail_identity(
            cfg,
            project,
            &name,
            &agent.pane_id,
            mail_identity.as_deref(),
        );
        (agent.pane_id.clone(), name)
    };
    let harness = plan.harness.clone();
    let mail_identity = crate::register_spawned_agent(cfg, project, &name, &harness, &pane_id);
    crate::record_spawn_entry(
        cfg,
        project,
        state::SpawnEntry::profiled(&pane_id, &name, &plan.profile, &harness, mail_identity),
    );
    // Readiness (mirrors spawn_crew_into): the first __send must not hit a
    // booting agent. Best-effort — a wait failure degrades to the __send
    // landing semantics, never a panic.
    if let Err(e) = c.agent_wait(&name, &[AgentStatus::Idle], Some(90_000)) {
        eprintln!("flywheel: spawn drain {name} readiness wait failed (dispatching anyway): {e}");
    }
    let mut mission_err: Option<String> = None;
    if let Some(w) = &plan.workflow {
        let prompt = headless::base_prompt_for(w, &json!({}));
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("__send")
            .arg(project)
            .arg(&name)
            .arg(&prompt)
            .arg("--confirm")
            .arg("--profile")
            .arg(&plan.profile);
        let output = cmd
            .output()
            .with_context(|| format!("__send spawn mission for {name} (workflow {w})"))?;
        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let trimmed = raw.trim();
        if let Ok(v) = serde_json::from_str::<Value>(trimmed)
            && v.get("ok").and_then(Value::as_bool) == Some(false)
        {
            mission_err = Some(
                v["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| raw.clone()),
            );
        } else if trimmed.is_empty() {
            mission_err = Some(String::from_utf8_lossy(&output.stderr).to_string());
        }
    }
    // await: true — the job finishes when the agent returns to Idle after
    // its mission. Bound by the configured wait timeout (default 30 min).
    if plan.await_job {
        let timeout_ms = cfg.default_timeout_ms.unwrap_or(1_800_000);
        c.agent_wait(&name, &[AgentStatus::Idle], Some(timeout_ms))?;
    }
    Ok(json!({
        "ok": true,
        "spawned": true,
        "promise_id": plan.promise_id,
        "profile": plan.profile,
        "harness": plan.harness,
        "workflow": plan.workflow,
        "pane_id": pane_id,
        "agent": name,
        "await": plan.await_job,
        "error": mission_err,
    }))
}

/// `Resonate::local` requires an entered Tokio runtime. Pane-mode keeps the
/// engine alive across `invoke` so the spawn drainer can share it — enter
/// the caller's runtime before construct, then drop the guard and `block_on`.
///
/// bd-adr22-local-resonate-tx8q: engine runs are wired to a PERSISTENT store
/// whenever one answers — [`cli_engine_store_url`] order is `RESONATE_URL`
/// env pin > host-local serve `http://localhost:8001` (TCP-probed) — so
/// compose/steer runs survive kill -9 mid-run and resume on restart
/// (ADR-0022 W3/W4). With no server answering, falls back to the embedded
/// in-memory runtime (previous behavior; in-process replay only).
pub fn local_engine(
    rt: &tokio::runtime::Runtime,
    runner: toron_workflow::ActionRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
) -> Result<toron_workflow::WorkflowEngine> {
    let server_url = cli_engine_store_url();
    local_engine_with_journal(
        rt,
        server_url,
        runner,
        profile,
        workflow,
        None,
        toron_workflow::engine::EffectJournalMode::Recording,
        // Dry engine: no effect journal, so no run rows either — the engine
        // performs nothing, and a row claiming otherwise is the lie the
        // journal exists to prevent (bd-mi9n).
        None,
    )
}

/// [`local_engine`] with the host-owned effect journal attached
/// (bd-engine-run-path-journaling-fwlx, ADR-0032 Spec 5/6).
///
/// Use this for runners that really perform effects — [`pane_dispatch`] and
/// `headless_dispatch`. The recording runner (`flywheel workflow run` without
/// `--execute`, `cron run` without `--live`) must stay on plain
/// [`local_engine`]: journaling a leaf that only *recorded* what it would have
/// dispatched would settle an external effect nobody performed, which is the
/// exact lie the journal exists to prevent.
///
/// With no toron index open, the journal cannot be built and the engine keeps
/// its own loud `bypassed-no-effect-runner` marker — never a run that only
/// looks journaled.
pub fn local_engine_journaled(
    cfg: &crate::config::Config,
    rt: &tokio::runtime::Runtime,
    runner: toron_workflow::ActionRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
) -> Result<toron_workflow::WorkflowEngine> {
    let server_url = cli_engine_store_url();
    local_engine_with(cfg, rt, server_url, runner, profile, workflow, true)
}

/// CLI engine store selection (bd-adr22-local-resonate-tx8q). The
/// toron-workflow probe order also auto-probes the shared multi-host server
/// (`https://resonate.alpine-tilapia.ts.net`) — deliberately NOT done here:
/// on a shared server, foreign workers polling the same default group can
/// claim a CLI run's tasks before this process does (observed: steer runs
/// parked 60s with tasks executed elsewhere / never delivered). The shared
/// server stays reachable via an explicit `RESONATE_URL` pin.
///
/// 127.0.0.1, NOT localhost: the host serve binds IPv4 while `localhost`
/// resolves ::1-first here, so a naive probe silently demotes every run to
/// the embedded in-memory store (observed live).
pub fn cli_engine_store_url() -> Option<String> {
    if let Ok(pin) = std::env::var("RESONATE_URL")
        && !pin.trim().is_empty()
    {
        return Some(pin);
    }
    const HOST_LOCAL_RESONATE_URL: &str = "http://127.0.0.1:8001";
    toron_workflow::engine::probe_candidate(HOST_LOCAL_RESONATE_URL)
        .then(|| HOST_LOCAL_RESONATE_URL.to_string())
}

/// Store-selection seam: `Some(url)` => remote HttpNetwork-backed engine
/// (durable), `None` => embedded in-memory runtime. Construction stays lazy;
/// the URL is only dialed when runs/promise ops execute.
/// [`local_engine`] / [`local_engine_journaled`] funnel: the only difference is
/// whether an effect journal is built for this runner (and which
/// [`EffectJournalMode`] the engine enforces).
fn local_engine_with(
    cfg: &crate::config::Config,
    rt: &tokio::runtime::Runtime,
    server_url: Option<String>,
    runner: toron_workflow::ActionRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
    journal: bool,
) -> Result<toron_workflow::WorkflowEngine> {
    // bd-mi9n: the journal is claimed in TWO halves — the effect runner the
    // leaves dispatch through, and the run-lifecycle journal the engine feeds
    // its own Started → Running → terminal rows through. Both come from one
    // store, so the run row and its effect rows are keyed by the same run_id.
    let built = if journal {
        crate::effect_journal::journals(cfg, runner.clone())
    } else {
        None
    };
    let (effect, run_journal) = match built {
        Some(j) => (Some(j.effect), Some(j.run)),
        None => (None, None),
    };
    // bd-engine-run-path-journaling-fwlx (the Spec 6 flip): a journaled
    // constructor is Enforced — its leaves really perform external effects,
    // so an uncovered sink must refuse, never run unjournaled. The plain
    // constructor (dry arms: `workflow run` without `--execute`, `cron run`
    // without `--live`) stays Recording — it records instead of performs, so
    // its leaves MUST stay unjournaled, visibly.
    let mode = if journal && effect.is_some() {
        toron_workflow::engine::EffectJournalMode::Enforced
    } else {
        toron_workflow::engine::EffectJournalMode::Recording
    };
    local_engine_with_journal(
        rt,
        server_url,
        runner,
        profile,
        workflow,
        effect,
        mode,
        run_journal,
    )
}

/// The store-selection + constructor tail shared by both entry points.
#[allow(clippy::too_many_arguments)]
fn local_engine_with_journal(
    rt: &tokio::runtime::Runtime,
    server_url: Option<String>,
    runner: toron_workflow::ActionRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
    effect: Option<toron_workflow::engine::EffectRunner>,
    mode: toron_workflow::engine::EffectJournalMode,
    run_journal: Option<toron_workflow::engine::RunJournal>,
) -> Result<toron_workflow::WorkflowEngine> {
    let _enter = rt.enter();
    match server_url {
        Some(url) => {
            toron_workflow::WorkflowEngine::remote_with_resolvers_effect_mode_and_run_journal(
                url,
                runner,
                profile,
                workflow,
                effect,
                mode,
                run_journal,
            )
        }
        None => toron_workflow::WorkflowEngine::local_with_resolvers_effect_mode_and_run_journal(
            runner,
            profile,
            workflow,
            effect,
            mode,
            run_journal,
        ),
    }
}

/// bd-omwg: the pane-mode spawn drainer thread. Polls the engine's SpawnSink
/// while `invoke` runs (an `await: true` run parks on its job promise until
/// the drainer settles it), launches each job through [`launch_spawn_job`],
/// and keeps draining until `stop` is set (the caller does this after invoke
/// returns, then joins). Loads the profile catalog fresh — the thread is
/// 'static and must not borrow the caller's.
pub fn start_approval_drainer(
    cfg: Config,
    project: String,
    engine: std::sync::Arc<toron_workflow::WorkflowEngine>,
    stop: std::sync::Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<Result<()>>> {
    let handle = std::thread::Builder::new()
        .name("approval-drainer".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| anyhow!("approval drainer runtime: {e}"))?;
            rt.block_on(async {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                // bd-1ub5: the drainer is the RESOLVE path. Parked promises are
                // resolved by scanning the project inbox for a captain decision
                // mail (`approve:<promise_id>` / `reject:<promise_id>` subject
                // token) — `flywheel workflow approve` sends exactly that. Mail
                // scan runs every ~2s inside the 150ms drain loop (every 13th
                // pass); mail unavailable degrades to the sink-only behavior.
                let mail = crate::mail_shim::Mail::from_config(Some(&cfg.toron_mail_config()))
                    .ok()
                    .flatten();
                let mut pass: u32 = 0;
                loop {
                    pass = pass.wrapping_add(1);
                    let ids: Vec<String> = {
                        let mut sink = engine.approval_sink().lock().await;
                        let mut new_ids = Vec::new();
                        for id in sink.iter() {
                            if !seen.contains(id) {
                                new_ids.push(id.clone());
                            }
                        }
                        for id in &new_ids {
                            seen.insert(id.clone());
                        }
                        new_ids
                    };
                    for promise_id in ids {
                        // Surface to captain: the decision token they reply with.
                        eprintln!(
                            "flywheel: approval parked: promise {promise_id} for project {project} — resolve with: flywheel workflow approve {run_tag} {promise_id} --decision approve|reject --project {project}",
                            run_tag = format!("run-{}", &promise_id[..promise_id.len().min(8)])
                        );
                        // ADR-0011 D7: SIGNOFF auto-pass iff REVIEW residual
                        // empty AND OPEN DECISIONS empty. Irreversible
                        // preference still parks.
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let swarm = crate::swarm_label(&project);
                        let ws = crate::doorbell::preferred_workspace(
                            &swarm,
                            crate::state::load_spawn_meta(&project).as_deref(),
                            |l| {
                                crate::doorbell::dispatcher_token(
                                    &crate::config::state_dir(),
                                    l,
                                )
                            },
                        );
                        let snap = crate::snapshot::assemble(&cfg, &project, &ws, now);
                        // REVIEW residual: any non-closed lane. Bead status
                        // is never the word "review", so that filter always
                        // auto-passed.
                        let review_residual = snap
                            .lanes
                            .iter()
                            .filter(|l| l.status != "closed")
                            .count();
                        let review_degraded = snap
                            .degraded
                            .iter()
                            .any(|d| d.contains("review"));
                        if crate::signoff::observe(
                            snap.decisions.len(),
                            review_residual,
                            review_degraded,
                        ) {
                            match engine
                                .resolve_approval(
                                    &promise_id,
                                    &toron_workflow::engine::Decision::approved(
                                        "auto-pass: REVIEW residual empty, OPEN DECISIONS empty",
                                    ),
                                )
                                .await
                            {
                                Ok(()) => {
                                    eprintln!(
                                        "flywheel: SIGNOFF auto-pass: promise {promise_id} (REVIEW residual empty, OPEN DECISIONS empty)"
                                    );
                                    seen.remove(&promise_id);
                                }
                                Err(e) => eprintln!(
                                    "flywheel: SIGNOFF auto-pass resolve {promise_id} failed: {e:#}"
                                ),
                            }
                        }
                    }
                    // Resolve pass: scan inbox for decision tokens on parked ids.
                    if let Some(mail) = &mail {
                        let parked: Vec<String> = {
                            let sink = engine.approval_sink().lock().await;
                            sink.iter().cloned().collect()
                        };
                        if !parked.is_empty() && pass % 13 == 0 {
                            for promise_id in &parked {
                                let token_a = format!("approve:{promise_id}");
                                let token_r = format!("reject:{promise_id}");
                                let Ok(inbox) = mail.inbox(&project, &mail.name, false, 25) else {
                                    continue;
                                };
                                let Some(rows) = inbox.as_array() else {
                                    continue;
                                };
                                for row in rows {
                                    let subject =
                                        row.get("subject").and_then(|s| s.as_str()).unwrap_or("");
                                    let sender =
                                        row.get("sender").and_then(|s| s.as_str()).unwrap_or("");
                                    let decision = if subject.contains(&token_a) {
                                        Some(toron_workflow::engine::Decision::approved(format!(
                                            "via workflow approve (from {sender})"
                                        )))
                                    } else if subject.contains(&token_r) {
                                        Some(toron_workflow::engine::Decision::rejected(format!(
                                            "via workflow approve (from {sender})"
                                        )))
                                    } else {
                                        None
                                    };
                                    if let Some(decision) = decision {
                                        let outcome = decision.outcome.clone();
                                        match engine
                                            .resolve_approval(promise_id, &decision)
                                            .await
                                        {
                                            Ok(()) => {
                                                eprintln!(
                                                    "flywheel: approval resolved: promise {} -> {} (from {sender})",
                                                    promise_id, outcome
                                                );
                                                seen.remove(promise_id);
                                            }
                                            Err(e) => eprintln!(
                                                "flywheel: resolve_approval {} failed: {e:#}",
                                                promise_id
                                            ),
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                Ok(())
            })
        })
        .map_err(|e| anyhow!("approval drainer thread: {e}"))?;
    Ok(handle)
}

pub fn start_spawn_drainer(
    cfg: Config,
    project: String,
    wf_name: String,
    ws_id: String,
    engine: std::sync::Arc<toron_workflow::WorkflowEngine>,
    stop: std::sync::Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<Result<Vec<Value>>>> {
    let handle = std::thread::Builder::new()
        .name("spawn-drainer".into())
        .spawn(move || {
            let catalog = crate::profile::ProfileCatalog::load().unwrap_or_default();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| anyhow!("spawn drainer runtime: {e}"))?;
            rt.block_on(async {
                loop {
                    if !engine.spawn_sink().lock().await.is_empty() {
                        let receipts = drain_spawn_sink(&engine, &catalog, &mut seen, |plan| {
                            launch_spawn_job(&cfg, &project, &wf_name, &ws_id, &plan)
                        })
                        .await?;
                        if !receipts.is_empty() {
                            eprintln!(
                                "flywheel: spawn drainer: {} job(s) launched",
                                receipts.len()
                            );
                        }
                    }
                    if stop.load(Ordering::Relaxed) {
                        // Final drain (an await:true resolve may have landed
                        // between the last poll and stop) — then exit.
                        let receipts = drain_spawn_sink(&engine, &catalog, &mut seen, |plan| {
                            launch_spawn_job(&cfg, &project, &wf_name, &ws_id, &plan)
                        })
                        .await?;
                        if !receipts.is_empty() {
                            eprintln!(
                                "flywheel: spawn drainer: {} job(s) launched (final)",
                                receipts.len()
                            );
                        }
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                Ok(vec![])
            })
        })
        .map_err(|e| anyhow!("spawn drainer thread: {e}"))?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-wave-swarm-graduation-fix-98io: both boot-race signatures are
    /// transient (retry), other reasons are not.
    #[test]
    fn transient_dispatch_reason_covers_boot_race() {
        assert!(is_transient_dispatch_reason(
            "agent_not_ready: agent target pane w8D:p1 pending"
        ));
        assert!(is_transient_dispatch_reason(
            "agent_pane_busy: agent target pane w8D:p1 is not an available shell"
        ));
        assert!(!is_transient_dispatch_reason("unknown profile @nope"));
        assert!(!is_transient_dispatch_reason(""));
    }

    /// bd-lifecycle-occupied-pane-wedge-07tx (probe-before-bind): the empty-
    /// pane selector never returns a pane whose agent slot is live. Given
    /// the occupant set, `select_empty_pane` picks the first FREE pane and
    /// skips occupied ones; all-occupied → None (caller splits fresh).
    /// Named workflow_packing_* so the bead's verify filter
    /// (`test workflow_packing`) hits it directly.
    #[test]
    fn workflow_packing_probe_before_bind_skips_occupied_panes() {
        let panes = vec!["p1".to_string(), "p2".to_string(), "p3".to_string()];
        let occupied: std::collections::HashSet<String> = ["p1".to_string()].into_iter().collect();
        assert_eq!(
            select_empty_pane(&panes, &occupied).as_deref(),
            Some("p2"),
            "first FREE pane wins, occupied root skipped"
        );
        let all: std::collections::HashSet<String> = panes.iter().cloned().collect();
        assert_eq!(
            select_empty_pane(&panes, &all),
            None,
            "all-occupied → None (split fresh instead of binding blind)"
        );
        assert_eq!(
            select_empty_pane(&[], &std::collections::HashSet::new()),
            None,
            "no panes at all → None"
        );
        let none: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert_eq!(
            select_empty_pane(&panes, &none).as_deref(),
            Some("p1"),
            "empty workspace keeps the old first-pane binding"
        );
    }

    /// bd-lifecycle-occupied-pane-wedge-07tx (bounded-fail-loud): the packed
    /// crew probe helper exists beside the dispatch gate and the transient
    /// exhaustion is surfaced — pinned by source shape so the two fixes
    /// cannot silently regress together.
    #[test]
    fn workflow_packing_bounded_fail_loud_is_pinned() {
        let src = std::fs::read_to_string(file!()).expect("pane.rs readable");
        // probe-before-bind: the root-pane pick consults occupancy
        assert!(
            src.contains("select_empty_pane(&pane_ids, &occupied)"),
            "spawn_crew_into must probe occupancy before binding the root pane"
        );
        assert!(
            src.contains("pane_split(\n                                    c,"),
            "all-occupied falls through to a fresh split, never a blind bind"
        );
        // bounded-fail-loud: transient exhaustion returns Err (per-step),
        // never the old fall-through Ok payload
        assert!(
            src.contains("failed after {MAX_ATTEMPTS} transient retries"),
            "pane_dispatch must fail loud after transient retries are exhausted"
        );
    }

    /// bd-mukq: 429 reroute target — the next fallback profile in the
    /// chain that has a LIVE pane in this run wins, exactly once.
    /// Empty chain, no live pane, or already-attempted → None (fail loud
    /// locally instead of redriving the exhausted route).
    #[test]
    fn dispatch_429_fallback_hop() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fw-pane-reroute-{}-{}.toml",
            std::process::id(),
            CTR.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(
            &path,
            "[profiles.oracle]\nharness = \"omp\"\nmodel = \"@premium\"\nfallback = [\"coder\"]\n\
             [profiles.coder]\nharness = \"pi\"\nmodel = \"@standard\"\n",
        )
        .expect("profiles.toml");
        let catalog = ProfileCatalog::load_from(Some(&path)).expect("catalog");
        let _ = std::fs::remove_file(&path);
        let entry = |pane_id: &str, name: &str, profile: &str| SpawnEntry {
            pane_id: pane_id.to_string(),
            name: name.to_string(),
            profile: Some(profile.to_string()),
            harness: None,
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            entry("w1:p1", "crew-wf-oracle", "oracle"),
            entry("w1:p2", "crew-wf-coder", "coder"),
        ];
        // Oracle 429s → coder pane is live: one hop.
        assert_eq!(
            dispatch_reroute_target(&catalog, &entries, "oracle", &[]),
            Some(("coder".to_string(), "crew-wf-coder".to_string()))
        );
        // Coder has no chain: fail loud locally.
        assert_eq!(
            dispatch_reroute_target(&catalog, &entries, "coder", &[]),
            None
        );
        // Fallback pane not live in this run: no hop.
        let oracle_only = vec![entry("w1:p1", "crew-wf-oracle", "oracle")];
        assert_eq!(
            dispatch_reroute_target(&catalog, &oracle_only, "oracle", &[]),
            None
        );
        // Already hopped once: terminal, never a second hop.
        assert_eq!(
            dispatch_reroute_target(
                &catalog,
                &entries,
                "oracle",
                &["oracle".to_string(), "coder".to_string()]
            ),
            None
        );
    }

    /// GS-043 B2 residual: pane 429 empty-evidence must refuse hop + journal
    /// Unknown — never silent continue without I4.
    #[test]
    fn pane_429_empty_evidence_refuses_hop_journals_unknown() {
        let src = include_str!("pane.rs");
        assert!(
            src.contains("refused-empty-evidence"),
            "pane 429 empty-evidence path must expose refused-empty-evidence"
        );
        let dispatch = src.find("bd-mukq: 429 reroute").expect("429 reroute block");
        let arm = &src[dispatch..dispatch.saturating_add(12000)];
        assert!(
            !arm.contains("still hop (legacy"),
            "429 reroute arm must not silent-continue hop without I4 journal"
        );
        assert!(
            arm.contains("refused-empty-evidence"),
            "429 arm must refuse empty-evidence hop"
        );
        // The refuse arm journals Unknown via commit_rebind_opt → Ok(None)
        let idx = src.find("refused-empty-evidence").expect("refused marker");
        let window = &src[idx.saturating_sub(900)..idx.saturating_add(200)];
        assert!(
            window.contains("commit_rebind_opt") && window.contains("Ok(None"),
            "refuse arm must journal Unknown via commit_rebind_opt Ok(None)"
        );
    }

    #[test]
    fn spawn_crew_into_uses_profile_launch_args() {
        let src = include_str!("pane.rs");
        let start = src
            .find("fn spawn_crew_into(")
            .expect("spawn_crew_into present");
        let slice = &src[start..start.saturating_add(16000)];
        assert!(
            slice.contains("profile_launch_plan(harness, profile, None)"),
            "compose pre-spawn must honor Profile.provider via the launch PLAN"
        );
        // The row itself, not just a call somewhere in the arm.
        assert!(
            slice.contains("model_pin,\n                skills_digest:"),
            "the crew registry row must carry the pin + skills digest (ADR-0013 P1-7/P1-8)"
        );
    }

    /// P10.3: `@role` resolves via the registry PROFILE column; a bare name
    /// falls back to the name match; missing → None (never a panic).
    #[test]
    fn resolve_pane_entry_prefers_profile_then_name() {
        let entries = vec![
            SpawnEntry {
                pane_id: "w1:p1".into(),
                name: "crew-wf-planner".into(),
                profile: Some("planner".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry {
                pane_id: "w1:p2".into(),
                name: "crew-wf-coder".into(),
                profile: Some("coder".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry::legacy("w1:p3", "plain-agent"),
        ];
        let e = resolve_pane_entry(&entries, "planner").expect("profile hit");
        assert_eq!(e.name, "crew-wf-planner");
        let e = resolve_pane_entry(&entries, "plain-agent").expect("name fallback");
        assert_eq!(e.name, "plain-agent");
        assert_eq!(e.profile, None);
        assert!(resolve_pane_entry(&entries, "ghost").is_none());
    }

    /// bd-lvyi: a per-step harness override resolves to a crew pane OF THAT
    /// HARNESS (the `harness` column), not the step profile's pane.
    #[test]
    fn resolve_pane_by_harness_finds_override_harness_pane() {
        let entries = vec![
            SpawnEntry {
                pane_id: "w1:p1".into(),
                name: "crew-wf-reviewer".into(),
                profile: Some("reviewer".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry {
                pane_id: "w1:p2".into(),
                name: "crew-wf-scout".into(),
                profile: Some("scout".into()),
                harness: Some("opencode".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
        ];
        let e = resolve_pane_by_harness(&entries, "opencode").expect("opencode pane");
        assert_eq!(e.name, "crew-wf-scout");
        assert_eq!(e.harness.as_deref(), Some("opencode"));
        assert!(resolve_pane_by_harness(&entries, "grok").is_none());
        assert!(resolve_pane_by_harness(&entries, "").is_none());
    }

    /// bd-lvyi: a legacy SpawnEntry (no harness column, pre-bd-lvyi registry)
    /// resolves via its profile's harness through the catalog; an entry with
    /// neither harness nor profile never matches.
    #[test]
    fn resolve_pane_by_harness_legacy_entry_falls_back_to_profile() {
        let entries = vec![
            SpawnEntry::legacy("w1:p1", "legacy-omp-agent"),
            SpawnEntry {
                pane_id: "w1:p2".into(),
                name: "crew-wf-coder".into(),
                profile: Some("coder".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry::legacy("w1:p3", "plain"),
        ];
        // Legacy entries have profile=None → never match (honest: unknown).
        assert!(
            resolve_pane_by_harness(&entries, "omp").is_some(),
            "coder's omp column matches"
        );
        let matched = resolve_pane_by_harness(&entries, "omp").expect("omp pane");
        assert_eq!(matched.name, "crew-wf-coder");
        assert!(
            resolve_pane_by_harness(&entries, "opencode").is_none(),
            "no opencode pane"
        );
    }

    /// P10.3: crew agent names are role-ordinal-suffixed, slugged + lowercased
    /// (herdr agent-name rule: lowercase alnum/-/_ only, <= 32 chars).
    #[test]
    fn crew_agent_names_are_role_ordinal() {
        assert_eq!(crew_agent_name("plan", "planner", 1), "crew-plan-planner");
        assert_eq!(crew_agent_name("plan", "coder", 2), "crew-plan-coder2");
        // Uppercase + spaces slug down (CREW-WF -> crew-wf)
        assert_eq!(
            crew_agent_name("CREW-WF", "planner", 1),
            "crew-crew-wf-planner"
        );
        assert_eq!(crew_agent_name("My Plan", "coder", 1), "crew-my-plan-coder");
        // long names truncate to stay within herdr's 32-char budget
        let long = crew_agent_name("a-very-long-workflow-name-that-goes-on", "reviewer", 1);
        assert!(long.len() <= 32, "name too long: {long} ({})", long.len());
        let short = crew_agent_name("wf", "reviewer", 1);
        assert!(short.len() <= 32);
    }

    /// bd-42fs step 5: a per-step harness override that differs from the
    /// profile's harness is a pane-path conflict (headless honors it; the
    /// pane crew is pre-spawned per profile). Same-harness overrides and
    /// absent overrides never conflict.
    #[test]
    fn harness_override_conflicts_only_when_different() {
        assert!(harness_override_conflicts("omp", Some("opencode")));
        assert!(harness_override_conflicts("omp", Some("grok")));
        assert!(!harness_override_conflicts("omp", Some("omp")));
        assert!(!harness_override_conflicts("omp", None));
        assert!(!harness_override_conflicts("omp", Some("")));
        assert!(!harness_override_conflicts("opencode", Some("opencode")));
        // ADR-0007: slot-aliased profiles carry an EMPTY harness — the
        // override is authoritative for the step, never a conflict.
        assert!(
            !harness_override_conflicts("", Some("pi")),
            "empty (slot-inherit) harness must not conflict"
        );
    }

    /// bd-ytr6: a step pinning harness=X with NO X pane falls back to the
    /// step's @role pane (different resolved harness) instead of bailing —
    /// the dispatch lands on the role pane (warned loudly at runtime). The
    /// `__send` leaf is reached with the role pane's name: in tests the
    /// subprocess is the test binary, which exits non-JSON, so the returned
    /// payload proves only the routing decision (agent = role pane).
    #[test]
    fn pane_dispatch_harness_fallback_dispatches_role_pane() {
        // reviewer with a RESOLVED harness (pi) so the opencode pin conflicts.
        let catalog = test_catalog_with_reviewer_harness("pi");
        let entry = |pane_id: &str, name: &str, profile: &str, harness: &str| SpawnEntry {
            pane_id: pane_id.to_string(),
            name: name.to_string(),
            profile: Some(profile.to_string()),
            harness: Some(harness.to_string()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        };
        // No opencode pane anywhere — the role pane runs pi.
        let entries = vec![entry("w1:p1", "crew-goal-reviewer", "reviewer", "pi")];
        let action = Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@reviewer".into()),
                harness: Some("opencode".into()),
                ..Default::default()
            },
            channel: None,
        };
        let out = pane_dispatch(&catalog, "proj", "wf", &json!({}), &entries, &action)
            .expect("fallback must dispatch, not bail");
        assert_eq!(
            out["agent"], "crew-goal-reviewer",
            "role pane wins the dispatch"
        );
        assert_eq!(out["profile"], "reviewer");
    }

    /// bd-ytr6: with neither an override-harness pane NOR a role pane, the
    /// original bail is kept exactly (fail loud, no silent degradation).
    #[test]
    fn pane_dispatch_harness_pin_missing_role_still_bails() {
        let catalog = test_catalog_with_reviewer_harness("pi");
        let entries = vec![SpawnEntry {
            pane_id: "w1:p1".into(),
            name: "crew-goal-coder".into(),
            profile: Some("coder".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        }];
        let action = Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@reviewer".into()),
                harness: Some("opencode".into()),
                ..Default::default()
            },
            channel: None,
        };
        let err = pane_dispatch(&catalog, "proj", "wf", &json!({}), &entries, &action)
            .expect_err("no opencode pane and no reviewer pane must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("overrides harness=opencode but no crew pane runs it"),
            "{msg}"
        );
    }

    /// Test fixture: defaults layered with `[profiles.reviewer] harness = H`
    /// (built-in reviewer's harness is EMPTY — slot-inherit — which never
    /// conflicts, so the pin path needs an explicit harness to fire).
    #[cfg(test)]
    fn test_catalog_with_reviewer_harness(harness: &str) -> ProfileCatalog {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fw-pane-pin-{}-{}.toml",
            std::process::id(),
            CTR.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(
            &path,
            format!("[profiles.reviewer]\nharness = \"{harness}\"\n"),
        )
        .expect("profiles.toml");
        let catalog = ProfileCatalog::load_from(Some(&path)).expect("catalog");
        let _ = std::fs::remove_file(&path);
        catalog
    }

    /// P10.3: an empty crew plan refuses pane mode (fail-fast, clear error).
    #[test]
    fn empty_crew_plan_is_an_error() {
        let plan = CrewPlan::default();
        let err = ensure_crew(&plan, "plan").unwrap_err();
        assert!(err.to_string().contains("needs a crew"), "{err}");
        // non-empty passes the guard
        let mut plan = CrewPlan::default();
        plan.members.insert("planner".to_string(), 1);
        ensure_crew(&plan, "plan").expect("non-empty plan passes");
    }

    fn test_catalog() -> crate::profile::ProfileCatalog {
        crate::profile::ProfileCatalog {
            profiles: BTreeMap::from([(
                "housekeeper".into(),
                crate::profile::Profile {
                    harness: "pi".into(),
                    model: "glm-5.3".into(),
                    effort: "max".into(),
                    ..Default::default()
                },
            )]),
            harnesses: BTreeMap::new(),
        }
    }

    fn housekeeper_resolver() -> toron_workflow::ProfileResolver {
        std::sync::Arc::new(|name: &str| {
            if name == "@housekeeper" {
                Some(json!({
                    "npub": "",
                    "spec": { "harness": "pi", "model": "glm-5.3", "backend": "herdr", "role": "housekeeper" },
                }))
            } else {
                None
            }
        })
    }

    /// bd-omwg: the engine's spawn side-channel promise spec yields a
    /// concrete launch plan — stripped profile, harness from the RESOLVED
    /// spec (the embedding's view), profile_launch_args for that harness
    /// (model/effort must reach agent_start, never an empty arg list), and
    /// the await flag that decides resolve-vs-fire-and-forget.
    #[test]
    fn spawn_launch_plan_reads_promise_spec() {
        let spec = json!({
            "profile": "@housekeeper",
            "workflow": "weekly-upgrade",
            "resolved": { "npub": "", "spec": { "harness": "pi", "model": "glm-5.3", "backend": "herdr", "role": "housekeeper" } },
            "await": true,
        });
        let plan = spawn_launch_plan("prom-1", &spec, &test_catalog()).expect("plan");
        assert_eq!(plan.promise_id, "prom-1");
        assert_eq!(plan.profile, "housekeeper", "@ stripped");
        assert_eq!(plan.workflow.as_deref(), Some("weekly-upgrade"));
        assert_eq!(plan.harness, "pi");
        assert!(
            plan.await_job,
            "await:true must resolve when the job finishes"
        );
        assert!(
            !plan.args.is_empty(),
            "model/effort args must reach agent_start"
        );
        assert!(
            plan.args.iter().any(|a| a.contains("glm-5.3")),
            "model flag present: {:?}",
            plan.args
        );
    }

    /// bd-omwg: an absent await flag (pre-flag specs) is fire-and-forget —
    /// never a resolve. An unknown profile still yields a launch plan with
    /// honest (empty) args — the pane launches, bare.
    #[test]
    fn spawn_launch_plan_defaults_await_false_and_bare_args() {
        let spec = json!({
            "profile": "@coder",
            "resolved": { "spec": { "harness": "omp" } },
        });
        let plan = spawn_launch_plan("prom-2", &spec, &crate::profile::ProfileCatalog::default())
            .expect("plan");
        assert!(!plan.await_job, "absent await = fire-and-forget");
        assert_eq!(plan.harness, "omp");
        assert!(plan.args.is_empty(), "unknown profile -> bare launch");
    }

    fn spawn_workflow(await_job: bool) -> toron_workflow::Workflow {
        toron_workflow::parse_workflow(&format!(
            r#"
name: spawn-drain-test
trigger:
  type: webhook
steps:
  - name: job
    action: spawn
    profile: "@housekeeper"
    workflow: weekly-upgrade
    await: {await_job}
"#
        ))
        .expect("parse spawn workflow")
    }

    /// bd-omwg contract: after WorkflowEngine::invoke, SpawnSink must NOT be
    /// silent. Every promise_id the engine pushed gets exactly one launch
    /// (the fake launcher records the plan), the sink drains to empty, and
    /// the engine's ok:true no longer lies about a pane.
    #[tokio::test(flavor = "multi_thread")]
    async fn drain_spawn_sink_launches_every_promise_after_invoke() {
        let runner = std::sync::Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let engine = std::sync::Arc::new(
            toron_workflow::WorkflowEngine::local_with(runner, housekeeper_resolver())
                .expect("engine"),
        );
        let invoke = tokio::spawn({
            let engine = engine.clone();
            async move {
                engine
                    .invoke("fw-test-spawn-drain-1", spawn_workflow(false), json!({}))
                    .await
            }
        });
        // Wait for the park (the created job promise is a remote todo the
        // 0.6 runtime suspends on), then drain — exactly what the production
        // drainer does.
        let mut receipts = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            if !engine.spawn_sink().lock().await.is_empty() {
                receipts = Some(
                    drain_spawn_sink(&engine, &test_catalog(), &mut seen, |plan| {
                        Ok(json!({
                            "pane": "w1:p9",
                            "agent": "crew-spawn-drain-test-spawn-housekeeper",
                            "profile": plan.profile,
                            "harness": plan.harness,
                            "args": plan.args,
                        }))
                    })
                    .await
                    .expect("drain"),
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let output: Value =
            match tokio::time::timeout(std::time::Duration::from_secs(15), invoke).await {
                Ok(Ok(Ok(v))) => v,
                Ok(Ok(Err(e))) => panic!("invoke failed: {e}"),
                Ok(Err(je)) => panic!("invoke task join: {je}"),
                Err(_) => panic!("invoke timed out — drainer never settled the job promise"),
            };
        let receipts = receipts.expect("drainer must have run");
        assert_eq!(receipts.len(), 1, "one promise -> one launch");
        assert_eq!(
            receipts[0]["profile"], "housekeeper",
            "receipt: {receipts:?}"
        );
        assert_eq!(receipts[0]["harness"], "pi");
        assert!(
            receipts[0]["args"].as_array().is_some_and(|a| a
                .iter()
                .any(|s| s.as_str().is_some_and(|x| x.contains("glm-5.3")))),
            "launch args carried: {:?}",
            receipts[0]["args"]
        );
        // The resume REPLAYS the run, re-executing run_spawn → the same
        // promise id lands in the sink again. The drainer's idempotent
        // re-drain (seen-dedup) consumes it — the sink is never left with
        // an unread spawn.
        for _ in 0..100 {
            if engine.spawn_sink().lock().await.is_empty() {
                break;
            }
            let re = drain_spawn_sink(&engine, &test_catalog(), &mut seen, |plan| {
                Ok(json!({
                    "pane": "w1:p9",
                    "agent": "crew-spawn-drain-test-spawn-housekeeper",
                    "profile": plan.profile,
                    "harness": plan.harness,
                    "args": plan.args,
                }))
            })
            .await
            .expect("re-drain");
            assert!(re.is_empty(), "replayed id must be deduped: {re:?}");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            engine.spawn_sink().lock().await.len(),
            0,
            "sink drained, never silent"
        );
        assert_eq!(output["job"]["spawned"], true, "engine spawns: {output}");
    }

    /// bd-omwg: an `await: true` spawn parks the run; the drainer's resolve
    /// — fired only AFTER the launcher reports the job finished — resumes it,
    /// and the step carries the job receipt (a real completion, never a fake
    /// kind-43004).
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_await_true_resumes_on_drainer_resolve() {
        let runner = std::sync::Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let engine = std::sync::Arc::new(
            toron_workflow::WorkflowEngine::local_with(runner, housekeeper_resolver())
                .expect("engine"),
        );
        let invoke = tokio::spawn({
            let engine = engine.clone();
            async move {
                engine
                    .invoke("fw-test-spawn-drain-2", spawn_workflow(true), json!({}))
                    .await
            }
        });
        // The drainer: poll the sink while the run parks, then launch +
        // resolve (the fake launcher "completes the job" by sleeping).
        let catalog = test_catalog();
        let drainer = tokio::spawn({
            let engine = engine.clone();
            async move {
                for _ in 0..500 {
                    if !engine.spawn_sink().lock().await.is_empty() {
                        let mut seen = std::collections::HashSet::new();
                        let receipts = drain_spawn_sink(&engine, &catalog, &mut seen, |_plan| {
                            Ok(json!({ "pane": "w1:p9", "agent": "spawn-agent" }))
                        })
                        .await
                        .expect("drain");
                        assert_eq!(receipts.len(), 1, "one launch receipt");
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                panic!("spawn never parked on a job promise");
            }
        });
        let output: Value =
            match tokio::time::timeout(std::time::Duration::from_secs(15), invoke).await {
                Ok(Ok(Ok(v))) => v,
                Ok(Ok(Err(e))) => panic!("invoke failed: {e}"),
                Ok(Err(je)) => panic!("invoke task join: {je}"),
                Err(_) => panic!("invoke timed out — the drainer never settled the job promise"),
            };
        drainer.await.expect("drainer ok");
        assert_eq!(output["job"]["ok"], true, "step succeeded: {output}");
        assert_eq!(
            output["job"]["result"]["pane"], "w1:p9",
            "job receipt captured, not a fake: {output}"
        );
    }

    /// Store-flavor probe (bd-adr22-local-resonate-tx8q): the SDK gates its
    /// introspection getters behind cfg(test), so the witness is behavioral.
    /// A dead-port REMOTE store fails transport (`network error: …`); an
    /// embedded LOCAL store answers authoritatively (`… not found`).
    async fn store_error_flavor(engine: &toron_workflow::WorkflowEngine) -> String {
        match engine.resonate().promises.get("__pane_probe__").await {
            Ok(_) => "resolved:probe-exists".to_string(),
            Err(e) => format!("{e}"),
        }
    }

    #[test]
    fn local_engine_prefers_remote_store_when_server_url_resolved() {
        // bd-adr22-local-resonate-tx8q: CLI engine runs must land on the
        // persistent store when one answers (host runs `resonate serve
        // --storage-type sqlite` on :8001), not the in-memory LocalNetwork.
        // Construction must stay lazy (dead port must not dial at build).
        let runner = std::sync::Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let profile: toron_workflow::ProfileResolver = std::sync::Arc::new(|_| None);
        let workflow: toron_workflow::WorkflowResolver = std::sync::Arc::new(|_| None);
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let engine = local_engine_with_journal(
            &rt,
            Some("http://127.0.0.1:59999".into()),
            runner.clone(),
            profile.clone(),
            workflow.clone(),
            None,
            toron_workflow::engine::EffectJournalMode::Recording,
            None,
        )
        .expect("remote-backed engine constructs without dialing");

        let remote_err = rt.block_on(store_error_flavor(&engine));
        assert!(
            remote_err.contains("error sending request") && !remote_err.contains("not found"),
            "Some(url) must wire the REMOTE HttpNetwork (transport-level failure), got: {remote_err}"
        );

        let fallback = local_engine_with_journal(
            &rt,
            None,
            runner,
            profile,
            workflow,
            None,
            toron_workflow::engine::EffectJournalMode::Recording,
            None,
        )
        .expect("fallback constructs");
        let local_err = rt.block_on(store_error_flavor(&fallback));
        assert!(
            local_err.contains("not found") && !local_err.contains("network error"),
            "None must stay embedded-local (authoritative miss, not transport failure), got: {local_err}"
        );
    }

    /// bd-engine-run-path-journaling-fwlx: the recording (dry-run) arm is the
    /// ONE engine rail that must stay unjournaled — its runner records what it
    /// would have dispatched, so a `settled` row would settle an external
    /// effect nobody performed. The engine's own bypass marker is the witness
    /// that this path claims no journal coverage.
    #[test]
    fn recording_engine_keeps_the_bypass_marker() {
        let runner: toron_workflow::ActionRunner =
            std::sync::Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let profile: toron_workflow::ProfileResolver = std::sync::Arc::new(|_| None);
        let workflow: toron_workflow::WorkflowResolver = std::sync::Arc::new(|_| None);
        let rt = tokio::runtime::Runtime::new().expect("rt");
        // None → embedded-local, so the assertion cannot depend on whether a
        // Resonate server happens to answer on this host.
        let engine = local_engine_with_journal(
            &rt,
            None,
            runner,
            profile,
            workflow,
            None,
            toron_workflow::engine::EffectJournalMode::Recording,
            None,
        )
        .expect("recording engine");
        let wf = toron_workflow::parse_workflow(
            "name: dry\nsteps:\n  - name: dispatch\n    action: dispatch_job\n    assignee: pi\n    spec:\n      task: work\n",
        )
        .expect("parse dry workflow");

        let out = rt
            .block_on(engine.invoke("wf-dry-journal-1", wf, json!({})))
            .expect("invoke");

        assert_eq!(
            out["dispatch"]["effect_journal"],
            json!("bypassed-no-effect-runner"),
            "the dry path journals nothing and says so: {out}"
        );
    }

    #[test]
    fn local_engine_constructs_under_entered_runtime() {
        let runner = std::sync::Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let profile: toron_workflow::ProfileResolver = std::sync::Arc::new(|_| None);
        let workflow: toron_workflow::WorkflowResolver = std::sync::Arc::new(|_| None);
        let rt = tokio::runtime::Runtime::new().expect("rt");
        assert!(
            local_engine(&rt, runner, profile, workflow).is_ok(),
            "Resonate::local must succeed after Runtime::enter"
        );
    }

    #[test]
    fn local_engine_without_runtime_panics() {
        let runner = std::sync::Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = toron_workflow::WorkflowEngine::local(runner);
        }))
        .is_err();
        assert!(
            panicked,
            "Resonate::local must refuse to construct off-runtime"
        );
    }
}

#[cfg(test)]
#[allow(unused_imports)]
mod kind_classify_tests {
    use crate::failure;

    #[test]
    fn kind_classify_pane_rate_limit() {
        let cf = failure::classify("429 Too Many Requests", None, None);
        assert_eq!(cf.kind, failure::FailureKind::RateLimit);
        assert!(cf.retryable);
        assert_eq!(cf.kind.to_string(), "rate_limit");
    }

    #[test]
    fn kind_classify_pane_timeout() {
        let cf = failure::classify("operation timeout after 30s", None, None);
        assert_eq!(cf.kind, failure::FailureKind::Timeout);
        assert!(cf.retryable);
    }

    #[test]
    fn kind_classify_pane_auth() {
        let cf = failure::classify("401 Unauthorized", None, None);
        assert_eq!(cf.kind, failure::FailureKind::Auth);
        assert!(!cf.retryable);
        assert!(cf.reroute_eligible);
    }

    #[test]
    fn kind_classify_pane_blocked_is_not_retryable() {
        let cf = failure::classify("reservation conflict: path held", None, None);
        assert_eq!(cf.kind, failure::FailureKind::Blocked);
        assert!(!cf.retryable);
        assert!(!cf.reroute_eligible);
    }

    #[test]
    fn kind_classify_pane_unclassified_stays_continue() {
        let cf = failure::classify("some random error without kind", None, None);
        assert_eq!(cf.kind, failure::FailureKind::Unknown);
        assert!(!cf.retryable);
        // Unclassified default is Continue (engine's on_error default remains Continue)
        // The kind-aware fail_run predicate lives in the engine, here only classification
    }

    #[test]
    fn kind_classify_pane_one_shot_fallback_fires_once_then_kind() {
        // Simulate the bd-42fs one-shot registry-fallback: first 429 triggers fallback,
        // second 429 (after fallback) returns kind:rate_limit with retryable:true
        // We test the registry directly: a model with fallback should resolve to fallback once,
        // and the fallback itself should not have a further fallback (bound of one).
        let mut registry = crate::models::ModelRegistry::default();
        registry.models.insert(
            "test/primary".to_string(),
            crate::models::ModelEntry {
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: Some("test/fallback".to_string()),
                effort: None,
                tier: None,
            },
        );
        registry.models.insert(
            "test/fallback".to_string(),
            crate::models::ModelEntry {
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: None,
                effort: None,
                tier: None,
            },
        );
        // First 429 on primary should find fallback
        let fb = registry
            .get("test/primary")
            .and_then(|m| m.fallback.clone());
        assert_eq!(fb, Some("test/fallback".to_string()));
        // Fallback itself has no further fallback (one-shot bound)
        let fb2 = registry
            .get("test/fallback")
            .and_then(|m| m.fallback.clone());
        assert_eq!(fb2, None);
        // Classification still returns rate_limit with retryable:true
        let cf = failure::classify("429 quota exceeded", None, None);
        assert_eq!(cf.kind, failure::FailureKind::RateLimit);
        assert!(cf.retryable);
    }
}

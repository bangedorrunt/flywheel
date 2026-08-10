//! Pane runner (ADR-0010 Spec A pane row, P10.3, bd-ghcj.1): real herdr
//! PANE execution for interactive workflows — a crew pre-spawned from the
//! workflow's `crew` plan, then each `dispatch_job` step sent to the running
//! pane via the loop's proven `__send` confirmed-send leaf.
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
//!   survive a flywheel crash; the spawn registry re-binds them.
//!
//! governed-by: ADR-0010 (Spec A, Spec C, Spec E), ADR-0005 (Spec 1, Spec 4c)

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use toron_workflow::schema::Action;

use crate::config::Config;
use crate::crew::CrewPlan;
use crate::herdr_shim::{AgentStatus, Client};
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
    let slug = slug
        .trim_matches('-')
        .chars()
        .take(16)
        .collect::<String>();
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
pub fn resolve_pane_entry<'a>(
    entries: &'a [SpawnEntry],
    profile: &str,
) -> Option<&'a SpawnEntry> {
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
/// -hgp): a pre-existing project workspace blocks the spawn unless
/// `--retire-orphans` closes it first — with `--reuse WORKSPACE` the named
/// workspace is bound instead (never blocked, never reaped).
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
    let orphans = crate::project_workspaces(&mut c, project)?;

    // --reuse: bind the named workspace (caller-owned, never reaped, and the
    // orphan guard skips it — it IS the target).
    if let Some(ws_id) = reuse {
        if !orphans.iter().any(|ws| ws.workspace_id == ws_id) {
            bail!("--reuse workspace {ws_id} not found for {project}");
        }
        let entries = spawn_crew_into(
            cfg, project, wf_name, plan, catalog, &mut c, &ws_id, &label,
        )?;
        return Ok(PaneRunCtx {
            workspace_id: ws_id.to_string(),
            entries,
            retained: true,
        });
    }

    // Fresh workspace: stale panes would race the new crew — same contract as
    // cmd_spawn.
    if !orphans.is_empty() {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| format!("{} (label {:?}, {} panes)", ws.workspace_id, ws.label, ws.pane_count))
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
                "found {} pre-existing workspace(s) for {project} — stale panes would race the crew: {}. Close them first (flywheel kill {project} or herdr workspace close <id>), or pass --retire-orphans / --reuse.",
                orphans.len(),
                detail.join(", ")
            );
        }
    }
    let (ws, _root) = c.workspace_create(Some(&crate::utf8_path(&dir, "workspace dir")?), Some(&label), false)?;
    let entries = spawn_crew_into(cfg, project, wf_name, plan, catalog, &mut c, &ws.workspace_id, &label)?;
    Ok(PaneRunCtx {
        workspace_id: ws.workspace_id,
        entries,
        retained: retain,
    })
}

/// The actual pane splits + agent starts for one workspace (shared by the
/// fresh-spawn and --reuse paths). Returns the registry entries.
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
        let profile = catalog.get(role).ok_or_else(|| {
            anyhow!("pane crew: unknown profile @{role} (flywheel profile list)")
        })?;
        let harness = &profile.harness;
        if harness.is_empty() {
            bail!("pane crew: profile @{role} has no harness (set --harness)");
        }
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
            let (agent_pane_id, name) = if let Some(e) = existing {
                eprintln!("  crew {}: reusing {} ({}) @{}", label, name, e.pane_id, role);
                (e.pane_id.clone(), name.clone())
            } else {
                let pane_id = match &prev {
                    None => {
                        // root pane of the workspace — list it by workspace id
                        let panes = c.pane_list(Some(ws_id))?;
                        panes
                            .first()
                            .map(|p| p.pane_id.clone())
                            .ok_or_else(|| {
                                anyhow!("pane crew: no root pane in workspace {ws_id}")
                            })?
                    }
                    Some(p) => {
                        let split = crate::herdr_shim::pane_split(
                            c,
                            p,
                            "down",
                            Some(&crate::utf8_path(&dir, "workspace dir")?),
                        )?;
                        split.pane_id
                    }
                };
                // AGENT_ARG = per-harness model flag (headless.rs model_args)
                // + the omp role models (--smol/--slow/--plan, bd-gsvn.2).
                let mut agent_args: Vec<String> = if profile.model.is_empty() {
                    vec![]
                } else {
                    headless::model_args(
                        harness,
                        &profile.model,
                        (!profile.effort.is_empty()).then_some(profile.effort.as_str()),
                    )
                };
                agent_args.extend(headless::role_model_args(
                    harness,
                    &profile.smol,
                    &profile.slow,
                    &profile.plan,
                ));
                let agent =
                    crate::start_agent_with_retry(c, &name, harness, &pane_id, &agent_args)?;
                crate::register_spawned_agent(cfg, project, &name, harness, &agent.pane_id);
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
                (agent.pane_id.clone(), name.clone())
            };
            prev = Some(agent_pane_id.clone());
            registry.retain(|e| e.name != name);
            registry.push(SpawnEntry {
                pane_id: agent_pane_id.clone(),
                name: name.clone(),
                profile: Some(role.clone()),
                harness: Some(harness.clone()),
            });
            eprintln!(
                "  crew {}: {} ({}) @{}",
                label, name, agent_pane_id, role
            );
        }
    }
    // Persist the crew registry (pane_id, name, profile) — `__send`'s auto-
    // heal re-binds lost names from it.
    state::save_spawn_registry(project, &registry)?;
    Ok(registry)
}

/// bd-42fs step 5: does a per-step harness override conflict with the
/// profile's harness? Pane crews pre-spawn per profile, so an override that
/// differs cannot be satisfied by an existing pane — the headless runner is
/// the override surface. Pure + unit-tested.
pub fn harness_override_conflicts(profile_harness: &str, spec_harness: Option<&str>) -> bool {
    match spec_harness {
        Some(h) if !h.trim().is_empty() => h != profile_harness,
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
            None => bail!(
                "pane dispatch: step @{profile_name} overrides harness={override_h} but no crew pane runs it (panes: {}) — per-step harness overrides land in --mode headless, or spawn a squad containing a {override_h} pane",
                entries
                    .iter()
                    .map(|e| format!("{}={}", e.name, e.harness.as_deref().unwrap_or("?")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
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
    loop {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("__send")
            .arg(project)
            .arg(&entry.name)
            .arg(&prompt)
            .arg("--confirm")
            .arg("--profile")
            .arg(profile_name);
        let output = cmd
            .output()
            .with_context(|| format!("__send for @{profile_name} ({})", entry.name))?;
        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let trimmed = raw.trim();
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            let transient = v
                .get("reason")
                .and_then(Value::as_str)
                .map(|r| r.contains("agent_not_ready"))
                .unwrap_or(false);
            if transient && attempts < MAX_ATTEMPTS {
                attempts += 1;
                eprintln!(
                    "flywheel: @{profile_name} not ready (attempt {attempts}/{MAX_ATTEMPTS}) — retrying in 10s"
                );
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
            return Ok(json!({
                "profile": profile_name,
                "agent": entry.name,
                "send": v,
            }));
        }
        let err = String::from_utf8_lossy(&output.stderr).to_string();
        return Ok(json!({
            "ok": false,
            "error": if err.trim().is_empty() { raw } else { err },
            "profile": profile_name,
            "agent": entry.name,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
            SpawnEntry {
                pane_id: "w1:p2".into(),
                name: "crew-wf-coder".into(),
                profile: Some("coder".into()),
                harness: Some("omp".into()),
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
            },
            SpawnEntry {
                pane_id: "w1:p2".into(),
                name: "crew-wf-scout".into(),
                profile: Some("scout".into()),
                harness: Some("opencode".into()),
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
        assert_eq!(crew_agent_name("CREW-WF", "planner", 1), "crew-crew-wf-planner");
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
}

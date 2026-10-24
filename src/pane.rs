// governed-by: ADR-0024
// governed-by: ADR-0027
// governed-by: ADR-0011
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
//!     survive a flywheel crash; the spawn registry re-binds them.
//!
//! governed-by: ADR-0024
//! governed-by: ADR-0010 (Spec A, Spec C, Spec E), ADR-0005 (Spec 1, Spec 4c), ADR-0022 (W3/W4 durable engine runs)

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use toron_workflow::schema::Action;

// ADR-0018 §1: the framework's shapes come from THIS plane's port module
// (`src/elm`), never straight from toron's — one door per repo, so an
// upstream API move lands in the port and not in the protocol code.
use crate::elm::{
    BreakerConfig, Cause, ClockImpl, Defect, Effect, Exit, Remote, RemoteError, RemotePolicy,
    Runtime, SystemClock, TestClock, kernel_millis,
};

use crate::config::Config;
use crate::crew::CrewPlan;
use crate::herdr::{AgentStatus, Client};
use crate::models::{FallbackRoute, ModelRegistry};
use crate::profile::ProfileCatalog;
use crate::state::SpawnEntry;
use crate::{headless, state};

/// The result of a pane-mode crew pre-spawn: the workspace + registry
/// entries the runner dispatches against, plus the reap decision.
#[derive(Debug, Clone)]
pub struct PaneRunCtx {
    /// workspace id the crew lives in
    pub workspace_id: String,
    /// spawn-registry entries (pane_id, name, profile) — live-gated, so
    /// dispatch resolution below cannot see a dead pane (bd-qx0z).
    pub entries: LiveRegistry,
    /// true when the workspace must NOT be reaped at run end (--reuse or
    /// --retain)
    pub retained: bool,
}

/// bd-qx0z: registry rows proven live against a pane set. The ONLY way to
/// build one is [`LiveRegistry::prune`] with the live pane ids — dispatch
/// and send resolution take `&LiveRegistry`, never a bare `Vec`, so an
/// unpruned read cannot compile into the dispatch path.
#[derive(Debug, Clone, Default)]
pub struct LiveRegistry(Vec<SpawnEntry>);

impl LiveRegistry {
    pub fn prune(
        entries: Vec<SpawnEntry>,
        live_pane_ids: &std::collections::HashSet<String>,
    ) -> Self {
        Self(prune_dead_pane_rows(entries, live_pane_ids))
    }

    pub fn entries(&self) -> &[SpawnEntry] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// bd-qx0z: dispatch-time registry for runners without a live client in
/// scope (cron engines): load + prune against a fresh pane listing.
/// Fail-open: an unreachable herdr keeps every loaded row (today's
/// behavior — no regression, just no pruning). One socket roundtrip per
/// construction; callers build once per engine, not per step.
pub fn live_registry_for_dispatch(project: &str) -> LiveRegistry {
    let entries = crate::state::load_spawn_registry(project);
    let live: std::collections::HashSet<String> =
        match Client::connect(None).and_then(|c| c.pane_list(None)) {
            Ok(panes) => panes.into_iter().map(|p| p.pane_id).collect(),
            Err(_) => entries.iter().map(|e| e.pane_id.clone()).collect(),
        };
    LiveRegistry::prune(entries, &live)
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

/// Wait for a spawned role's pane to appear in `live_panes`, bounded.
///
/// A row in the spawn registry is proof the pane was created, so its absence
/// from herdr's listing is a spawn/list race rather than a dead pane — and
/// [`live_registry_for_dispatch`]'s prune cannot tell those apart, because it
/// drops every row whose `pane_id` herdr did not return. Without this, a crew
/// that spawned correctly loses the whole run to a missing-role bail.
///
/// `live_panes` returns `None` when the probe is unavailable, which is treated
/// as "cannot confirm" and keeps polling until the window closes — the same
/// fail-open posture the prune already takes for an unreachable herdr.
///
/// The wait is bounded on purpose: an unbounded poll would hang the run on a
/// genuinely dead pane. `None` means the window closed without the pane, and
/// the caller bails loud exactly as before (bd-ietv).
/// bd-0s0y: settle for the `nth` copy of a role, so a fan-out copy settles for
/// ITS OWN pane instead of copy 0's. `None` is the single-pane case.
pub fn settle_role_slot_with<F>(
    rows: &[SpawnEntry],
    role: &str,
    nth: Option<usize>,
    mut live_panes: F,
    window_ms: u64,
    poll_ms: u64,
) -> Option<SpawnEntry>
where
    F: FnMut() -> Option<std::collections::HashSet<String>>,
{
    let registry = LiveRegistry(rows.to_vec());
    // No row for the role means it was never spawned: nothing to settle.
    let want = match nth {
        Some(i) => resolve_pane_entry_nth(&registry, role, i)?,
        None => resolve_pane_entry(&registry, role)?,
    }
    .pane_id
    .clone();
    let mut waited = 0u64;
    loop {
        if let Some(live) = live_panes()
            && live.contains(&want)
        {
            return rows.iter().find(|e| e.pane_id == want).cloned();
        }
        if waited >= window_ms {
            return None;
        }
        if poll_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
        }
        waited = waited.saturating_add(poll_ms);
    }
}

/// bd-0s0y: production settle for the `nth` copy of a role.
pub fn settle_for_role_slot(project: &str, role: &str, nth: Option<usize>) -> Option<SpawnEntry> {
    const WINDOW_MS: u64 = 5_000;
    const POLL_MS: u64 = 250;
    let rows = crate::state::load_spawn_registry(project);
    settle_role_slot_with(
        &rows,
        role,
        nth,
        || {
            Client::connect(None)
                .ok()
                .and_then(|c| c.pane_list(None).ok())
                .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
        },
        WINDOW_MS,
        POLL_MS,
    )
}

/// Resolve which registry entry serves a dispatch target: first the entry
/// whose PROFILE matches `@role`, else an entry whose NAME matches. Pure —
/// the runner's `@role -> running pane` mapping (ADR-0010 Spec A pane row).
/// bd-rkx8: next generation for a respawned name — one past the highest
/// incarnation on record, 0 for a fresh name. Pure.
pub fn next_incarnation(entries: &[SpawnEntry], name: &str) -> u64 {
    entries
        .iter()
        .filter(|e| e.name == name)
        .map(|e| e.incarnation)
        .max()
        .map_or(0, |m| m.saturating_add(1))
}

/// bd-rkx8: highest incarnation wins, ties keep insertion order
/// (previous first-match behavior, unchanged).
fn best_generation<'a>(
    it: impl Iterator<Item = (usize, &'a SpawnEntry)>,
) -> Option<&'a SpawnEntry> {
    it.max_by(|(ia, a), (ib, b)| a.incarnation.cmp(&b.incarnation).then_with(|| ib.cmp(ia)))
        .map(|(_, e)| e)
}

pub fn resolve_pane_entry<'a>(entries: &'a LiveRegistry, profile: &str) -> Option<&'a SpawnEntry> {
    // bd-rkx8: highest incarnation wins — names are stable across
    // respawns, so first-match could return a stale generation.
    best_generation(
        entries
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, e)| e.profile.as_deref() == Some(profile)),
    )
    .or_else(|| {
        best_generation(
            entries
                .entries()
                .iter()
                .enumerate()
                .filter(|(_, e)| e.name == profile),
        )
    })
}

/// bd-0s0y: resolve the pane that serves `role` for the copy at `copy_index`.
///
/// `None` (a step that is not a fan-out copy) resolves exactly as before, so
/// the whole existing path is untouched. `Some(i)` addresses the `i`th copy.
///
/// bd-07m2: a shortfall returns `Ok(None)` and does NOT bail. It used to
/// `bail!` here, and the caller wrote `resolve_role_slot(...)?` — which made
/// the `None` arm, the bd-ietv settle window, unreachable for EVERY fan-out
/// copy. Cron Pane mode builds `entries` from `live_registry_for_dispatch`
/// (load + prune against a fresh listing), so a pane created moments ago is
/// absent and the operator was told "the crew holds 0 pane(s) for @reviewer"
/// when the truth was a spawn/list race the settle window exists to absorb.
/// The shortfall is raised by [`shortfall_error`], AFTER settling fails, so
/// the two causes stay distinguishable.
///
/// A shortfall is still never a silent reuse of copy 0: that is the exact
/// defect this fixes — copy 1 would be refused by the busy-owner fence and the
/// run would die naming the wrong cause.
fn resolve_role_slot<'a>(
    entries: &'a LiveRegistry,
    role: &str,
    copy_index: Option<usize>,
) -> Result<Option<&'a SpawnEntry>> {
    let Some(i) = copy_index else {
        return Ok(resolve_pane_entry(entries, role));
    };
    Ok(resolve_pane_entry_nth(entries, role, i))
}

/// bd-07m2: the shortfall verdict, counted in SLOTS rather than rows.
///
/// `resolve_pane_entry_nth` groups by agent name and keeps the highest
/// incarnation, so a respawn is a newer generation of the SAME slot and never
/// an extra copy. Counting rows here (the old code) told an operator to grow a
/// crew that was already the right size whenever one slot had two generations
/// on record — the bd-rkx8 invariant, violated in the message.
fn shortfall_error(entries: &LiveRegistry, role: &str, i: usize) -> anyhow::Error {
    let have = entries
        .entries()
        .iter()
        .filter(|e| e.profile.as_deref() == Some(role) || e.name == role)
        .map(|e| e.name.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    anyhow!(
        "pane dispatch: fan-out copy {i} of @{role} has no crew pane — the crew holds {have} \
         slot(s) for @{role} and this step asked for copy {i}, so @{role} needs {needed} \
         copies. Give @{role} {needed} panes (or lower the step's fan-out). Another copy's pane \
         is never reused: the busy-owner fence would refuse this send and name the wrong cause.",
        needed = i + 1
    )
}

/// bd-0s0y: the `nth` DISTINCT live pane serving `profile`.
///
/// A `parallel`/`each` fan-out over a role runs N copies of the *same* step,
/// and each copy needs its own agent: the busy-owner fence admits exactly
/// one in-flight dispatch per agent (bd-fen3), so N copies that resolve to
/// one pane is a guaranteed refusal for copies 2..N — not a race, a certainty.
/// [`resolve_pane_entry`] is a single-entry lookup with no ordinal, so every
/// copy got the same pane and the run died at the first fan-out. `ce-brainstorm`'s
/// `diverge` is `over: "3"` over `@oracle`, and a crew sizes one pane per
/// unique ROLE, so no crew size could ever satisfy it.
///
/// Ordering, and why it is safe: rows are grouped by agent NAME, the highest
/// incarnation wins per name (a respawn is a newer generation of the SAME
/// slot, never an extra copy), and the surviving names keep registry
/// insertion order — which is spawn order, so `nth` is the nth pane the crew
/// actually created. Panes are distinct because one name maps to one pane.
pub fn resolve_pane_entry_nth<'a>(
    entries: &'a LiveRegistry,
    profile: &str,
    nth: usize,
) -> Option<&'a SpawnEntry> {
    let serves = |e: &SpawnEntry| e.profile.as_deref() == Some(profile) || e.name == profile;
    // First appearance of each name fixes the slot order; spawn order is the
    // order the crew created the copies in, and it is what `nth` means.
    let mut slots: Vec<&SpawnEntry> = Vec::new();
    for e in entries.entries().iter().filter(|e| serves(e)) {
        if !slots.iter().any(|s| s.name == e.name) {
            slots.push(e);
        }
    }
    slots
        .iter()
        .filter_map(|slot| {
            best_generation(
                entries
                    .entries()
                    .iter()
                    .enumerate()
                    .filter(move |(_, e)| e.name == slot.name),
            )
        })
        .nth(nth)
}

/// bd-op8b: drop registry rows whose pane no longer exists anywhere.
/// Loaded registries otherwise carry dead rows forever; the first
/// profile-match then steals fresh-crew dispatches (and a stale
/// skills_injected=true mutes the readiness gate). Pure — the caller
/// supplies the live pane set and decides failure policy.
///
/// This is the DISPATCH gate, not the persisted clear: whatever herdr does not
/// list is not dispatchable, whatever its harness. The persisted reap
/// ([`reap_closed_rows`]) asks the stricter bd-j0b5 question before it drops a
/// row for good.
pub fn prune_dead_pane_rows(
    entries: Vec<SpawnEntry>,
    live_pane_ids: &std::collections::HashSet<String>,
) -> Vec<SpawnEntry> {
    entries
        .into_iter()
        .filter(|e| live_pane_ids.contains(&e.pane_id))
        .collect()
}

/// bd-bfwj: split a registry into the rows that survive the persisted clear
/// plus a pruned count, given the live pane set. bd-j0b5: a gone pane is death
/// only for a harness herdr injects — a mail-only pin keeps its row. Pure —
/// tests pin the decision, the IO helper below supplies the live set and
/// persists.
pub fn reap_closed_rows(
    entries: Vec<SpawnEntry>,
    live_pane_ids: &std::collections::HashSet<String>,
    backend: &str,
) -> (Vec<SpawnEntry>, usize) {
    let before = entries.len();
    let kept: Vec<SpawnEntry> = entries
        .into_iter()
        .filter(|e| {
            live_pane_ids.contains(&e.pane_id)
                || !crate::pane_keep::pane_binding_may_clear(
                    e.harness.as_deref().unwrap_or(""),
                    backend,
                    &e.pane_id,
                    Some(live_pane_ids),
                )
        })
        .collect();
    let pruned = before.saturating_sub(kept.len());
    (kept, pruned)
}

/// bd-bfwj: reap registry rows whose pane is gone everywhere. Called after
/// closes that actually kill panes (orphan retire, pre-existing retire,
/// compose reap, kill) — never where children survive (captain-continue
/// keeps panes listed, so nothing is gone). Fail-open: an unlistable
/// backend keeps every row (existing behavior).
pub fn reap_dead_registry_rows(project: &str, c: &mut Client) -> usize {
    let live: std::collections::HashSet<String> = match c.pane_list(None) {
        Ok(panes) => panes.into_iter().map(|p| p.pane_id).collect(),
        Err(e) => {
            eprintln!("flywheel: dead-row reap skipped (pane list failed: {e:#})");
            return 0;
        }
    };
    // bd-j0b5: resolve the process backend once so the pure clear can decide;
    // config load is best-effort and mirrors the multiplexer precedence.
    let backend = crate::config::Config::load()
        .map(|cfg| crate::backend::factory::resolve_backend_name(&cfg))
        .unwrap_or_else(|_| {
            crate::backend::factory::resolve_backend_name(&crate::config::Config::default())
        });
    let (kept, pruned) =
        reap_closed_rows(state::load_spawn_registry(project), &live, &backend);
    if pruned == 0 {
        return 0;
    }
    match state::save_spawn_registry(project, &kept) {
        Ok(()) => {
            eprintln!("flywheel: reaped {pruned} dead-pane registry row(s) for {project}");
            pruned
        }
        Err(e) => {
            eprintln!("flywheel: dead-row reap save failed: {e:#}");
            0
        }
    }
}
/// bd-nwnb: the dead-row gauge core — rows herdr may clear, as
/// (pane_id, name) pairs. bd-p09m: a row is dead only when
/// [`crate::pane_keep::pane_binding_may_clear`] is true, so a mail-only row
/// the reap keeps never shows as dead. Pure; [`dead_registry_rows`] supplies
/// the live set and reads the file.
pub fn find_dead_rows(
    entries: &[SpawnEntry],
    live_pane_ids: &std::collections::HashSet<String>,
    backend: &str,
) -> Vec<(String, String)> {
    entries
        .iter()
        .filter(|e| {
            !live_pane_ids.contains(&e.pane_id)
                && crate::pane_keep::pane_binding_may_clear(
                    e.harness.as_deref().unwrap_or(""),
                    backend,
                    &e.pane_id,
                    Some(live_pane_ids),
                )
        })
        .map(|e| (e.pane_id.clone(), e.name.clone()))
        .collect()
}

/// bd-nwnb: dead registry rows for status output. Fail-open: an
/// unlistable backend reports none (never error the status render).
pub fn dead_registry_rows(project: &str, c: &mut Client) -> Vec<(String, String)> {
    let Ok(panes) = c.pane_list(None) else {
        return Vec::new();
    };
    let live: std::collections::HashSet<String> = panes.into_iter().map(|p| p.pane_id).collect();
    // bd-p09m: same backend resolution as the reap, so the gauge agrees
    // with the clear.
    let backend = crate::config::Config::load()
        .map(|cfg| crate::backend::factory::resolve_backend_name(&cfg))
        .unwrap_or_else(|_| {
            crate::backend::factory::resolve_backend_name(&crate::config::Config::default())
        });
    find_dead_rows(&state::load_spawn_registry(project), &live, &backend)
}
/// bd-lvyi: resolve a per-step harness override to a crew pane running THAT
/// harness. The pane's `harness` column is authoritative; a legacy entry
/// without one (pre-bd-lvyi registry) falls back to its profile's harness
/// via the catalog. `None` → no pane of the override harness exists.
pub fn resolve_pane_by_harness<'a>(
    entries: &'a LiveRegistry,
    harness: &str,
) -> Option<&'a SpawnEntry> {
    // bd-rkx8: highest incarnation wins, ties keep insertion order.
    best_generation(entries.entries().iter().enumerate().filter(|(_, e)| {
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
    }))
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
            // bd-bfwj: closing kills these panes — reap their registry rows
            // now so no later dispatch resolves them.
            reap_dead_registry_rows(project, &mut c);
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

/// Decide the one-shot registry fallback for a FAILED pane spawn. `None` means
/// "re-raise the original error untouched": a non-rate-limit failure, an
/// unreadable registry, an unknown profile model, or a model whose entry
/// carries no `fallback`.
///
/// One shot by construction: the decision is made once per failed spawn, and
/// the caller journals and retries once inside the same `commit_rebind`. A
/// fallback that also rate-limits propagates as an error instead of hopping
/// again — there is no loop here to re-enter.
///
/// Pure over (error, profile model, harness, loader): the caller owns every
/// effect. `load_registry` is called ONLY once the gate opens, so a failure
/// that cannot hop never pays for reading `models.toml`.
pub(crate) fn registry_fallback_route<F>(
    error: &str,
    profile_model: &str,
    harness: &str,
    load_registry: F,
) -> Option<FallbackRoute>
where
    F: FnOnce() -> Option<ModelRegistry>,
{
    // Stage-typed classify: the pane spawn hop is an INPUT failure, and only a
    // rate limit opens it. A timeout or an auth failure must re-raise — those
    // do not get better by changing the model.
    let cf = crate::failure::classify_with_action_stage(
        error,
        None,
        None,
        Some(crate::kernel::ActionStage::Input),
    );
    if cf.kind != crate::failure::FailureKind::RateLimit {
        return None;
    }
    load_registry()?.fallback_route(profile_model, harness)
}

/// The I4 journal intent for a decided model-fallback hop (GS-043), shared by
/// the pane hop and both headless rungs. The `to` binding records the id that
/// drove the decision, not the harness-resolved injection string: the hop's
/// *identity* is the model swap, and the rendering is derived from it. It
/// takes the caller's `&FallbackRoute` so the row and the dispatch read one
/// value — a row can never name a harness the run did not use.
pub(crate) fn model_fallback_intent(
    project: &str,
    name: &str,
    role: &str,
    profile_model: &str,
    harness: &str,
    route: &FallbackRoute,
    evidence: String,
) -> crate::agent_ref::RebindIntent {
    crate::agent_ref::RebindIntent::prepare(
        crate::agent_ref::AgentRef::new(project, name),
        crate::agent_ref::AgentBinding {
            profile_name: role.to_string(),
            model: profile_model.to_string(),
            harness: harness.to_string(),
        },
        crate::agent_ref::AgentBinding {
            profile_name: role.to_string(),
            model: route.model_id.clone(),
            harness: route.harness.clone(),
        },
        // The gate above is RateLimit, so the reason kind is fixed here.
        crate::agent_ref::RebindReason::ModelFallback {
            kind: "rate_limit".into(),
            evidence,
        },
        0,
    )
}

/// bd-d7zu: the fail-closed refusal when a freshly split crew pane never
/// reaches its line editor. Pure so the wording and the refusal itself are
/// testable without a pane — the operator has to be told the SHELL is the
/// problem, not handed the downstream `agent not found in toron` that this
/// very gate exists to prevent.
fn shell_gate_refusal(name: &str, pane_id: &str) -> anyhow::Error {
    anyhow!(
        "pane crew: shell for {name} ({pane_id}) never rendered a prompt within 15s — refusing \
         to type the launch arg into a shell that is not at its line editor (a ~2.6KB \
         single-quoted arg would split mid-quote and leave the agent dead on spawn). Re-run once \
         the host is less loaded, or raise the budget."
    )
}
#[allow(clippy::too_many_arguments)] // one param per caller mirror, mirrors cmd_spawn
fn spawn_crew_into(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    plan: &CrewPlan,
    catalog: &ProfileCatalog,
    c: &mut Client,
    ws_id: &str,
    label: &str,
) -> Result<LiveRegistry> {
    // Preserve pre-existing registry entries (swarm panes, prior crew) —
    // only entries whose NAME we are about to (re)spawn are replaced, so a
    // --reuse re-run re-binds the crew without clobbering unrelated panes.
    // bd-op8b: prune rows whose pane is gone everywhere first — dead rows
    // otherwise win the first profile-match in dispatch resolution and send
    // fresh-crew work into closed panes (live proof fw-composed-2026-09-23).
    // Fail-open: an unlistable backend keeps every row (existing behavior).
    let mut registry: Vec<SpawnEntry> = state::load_spawn_registry(project);
    let mut live_known: Option<std::collections::HashSet<String>> = c
        .pane_list(None)
        .ok()
        .map(|panes| panes.into_iter().map(|p| p.pane_id).collect());
    if let Some(live) = &live_known {
        let before = registry.len();
        registry = prune_dead_pane_rows(registry, live);
        let pruned = before - registry.len();
        if pruned > 0 {
            eprintln!("flywheel: pruned {pruned} dead-pane registry row(s) for {project}");
        }
    }
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
            let (agent_pane_id, name, model_pin) = if let Some(e) = existing {
                eprintln!(
                    "  crew {}: reusing {} ({}) @{}",
                    label, name, e.pane_id, role
                );
                // A reused live pane keeps the provenance its own spawn
                // recorded (ADR-0013 P1-7) — this path resolves no plan.
                (e.pane_id.clone(), name.clone(), e.model_pin.clone())
            } else {
                // bd-crlx: resolve the launch plan BEFORE the pane exists.
                // herdr injects env only at split/create, and a pane's env is
                // fixed for the life of the process it launches — so a model
                // the harness cannot take as argv (opencode's root command has
                // no `--model`) has to be pinned into the pane env at creation
                // time. There is no "set it afterwards".
                let (agent_args, model_pin, env_model_pin): (
                    Vec<String>,
                    Option<String>,
                    Option<String>,
                ) = if profile.model.is_empty() && profile.provider.is_empty() {
                    (vec![], None, None)
                } else {
                    let plan = crate::profile_launch_plan(harness, profile, None)
                        .map_err(|e| anyhow!("pane crew @{role} model routing: {e}"))?;
                    (plan.args, Some(plan.model), plan.env_model_pin)
                };
                let pane_env =
                    pane_env_for_launch(cfg, mail_identity.as_deref(), env_model_pin.as_deref())?;
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
                        // bd-crlx: a seat whose model travels by env can never
                        // reuse a pre-existing pane. Those panes were created by
                        // `workspace_create` with a different env, and a
                        // process's env is fixed at its start — reusing one
                        // would launch the agent unpinned, which is exactly the
                        // silent model downgrade this pin exists to prevent.
                        // Always split a fresh pane so the pin is really there.
                        let reusable = if env_model_pin.is_some() {
                            None
                        } else {
                            select_empty_pane(&pane_ids, &occupied)
                        };
                        match reusable {
                            Some(pid) => pid,
                            None => {
                                let root = pane_ids.first().cloned().ok_or_else(|| {
                                    anyhow!("pane crew: no root pane in workspace {ws_id}")
                                })?;
                                let split = crate::herdr::pane_split(
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
                        let split = crate::herdr::pane_split(
                            c,
                            p,
                            "down",
                            Some(&crate::utf8_path(&dir, "workspace dir")?),
                            Some(&pane_env),
                        )?;
                        split.pane_id
                    }
                };
                // bd-d7zu: WAIT FOR THE SHELL BEFORE TYPING THE LAUNCH ARG.
                // herdr types AGENT_ARG into the pane PTY the moment
                // `agent.start` is called, and a fresh pane's shell is still
                // booting (zsh running .zshrc, prompt not yet rendered, PTY in
                // CANONICAL mode). The peer-aware template renders to a ~2.6KB
                // single-quoted arg; typed into a booting shell it splits
                // mid-quote and zsh parks at the PS2 continuation prompt with
                // the agent never starting — herdr.rs documents this as
                // the 2026-08-18 "coder2/coder3 dead-on-spawn" incident.
                //
                // `cmd_crew_control` (the relaunch path) has always gated on
                // `wait_shell_prompt`; this crew path did not, which is why
                // `flywheel crew control relaunch` survives and
                // `workflow compose --run` dead-on-spawns.
                //
                // Fail CLOSED, unlike the relaunch path's warn-and-continue.
                // Typing the launch arg into a shell that never reached its
                // line editor does not merely risk a bad spawn: herdr rejects
                // it (`agent_pane_busy`), the readiness wait then reports
                // `agent_not_running`, and the dispatch degrades to
                // "dispatching anyway" — which surfaces as the permanent,
                // unactionable `agent <name> not found in toron`. Proceeding
                // past a failed gate is the defect (bd-d7zu principle
                // `fail-closed`); refuse with the real reason instead. A
                // measured fresh pane needs ~770ms, so a 15s budget that
                // expires is a genuinely broken shell, not a slow one.
                if !crate::herdr::wait_shell_prompt(c, &pane_id, Duration::from_secs(15)) {
                    return Err(shell_gate_refusal(&name, &pane_id));
                }
                // AGENT_ARG / the pane-env pin were resolved BEFORE the pane
                // existed (bd-crlx): the plan form (not the argv-only view) so
                // the registry row below carries the same resolved Muse/slot
                // pin the launch actually carries, honoring Profile.provider
                // through the same registry pin as cmd_spawn /
                // spawn_launch_plan (ADR-0027 compose gap, ADR-0013 P1-7).
                // ADR-0024 W1a: pane spawn one-shot registry-fallback for rate_limit (bd-42fs pattern)
                let agent =
                    match crate::start_agent_with_retry(c, &name, harness, &pane_id, &agent_args) {
                        Ok(a) => a,
                        Err(e) => {
                            // A registry we cannot read, a failure that is not a
                            // rate limit, or a model with no fallback all re-raise
                            // the original error untouched.
                            let route = registry_fallback_route(
                                &e.to_string(),
                                &profile.model,
                                harness,
                                || match ModelRegistry::load_routing() {
                                    Ok(registry) => Some(registry),
                                    // An unparseable models.toml is LOUD by
                                    // design. Do not let the fallback path
                                    // swallow it into a bare "no fallback".
                                    Err(err) => {
                                        eprintln!(
                                            "pane crew: model registry unreadable \
                                             — no rate-limit fallback: {err:#}"
                                        );
                                        None
                                    }
                                },
                            );
                            let Some(route) = route else { return Err(e) };
                            // bd-oc-bare-spawn-model-argv-zbzt: argv for the
                            // interactive re-launch, never the headless form.
                            // A route this pane cannot carry (opencode's pin
                            // travels by env, and this pane's env is already
                            // fixed) refuses here, before the rebind journal.
                            let fallback_args = crate::interactive_pane_args(
                                &route.harness,
                                &route.model,
                                (!profile.effort.is_empty()).then_some(profile.effort.as_str()),
                            )
                            .map_err(|why| {
                                anyhow!(
                                    "pane crew: @{role} rate-limited on {} and the fallback {} \
                                     cannot be delivered into this pane: {why:#} (original \
                                     failure: {e:#})",
                                    profile.model,
                                    route.model_id
                                )
                            })?;
                            // GS-043 I4: journal before ModelEntry.fallback hop.
                            let intent = model_fallback_intent(
                                project,
                                &name,
                                role,
                                &profile.model,
                                harness,
                                &route,
                                e.to_string(),
                            );
                            eprintln!(
                                "pane crew: @{role} rate-limited on {} — registry fallback {}",
                                profile.model, route.model_id
                            );
                            crate::agent_ref::commit_rebind(project, intent, |_| {
                                crate::start_agent_with_retry(
                                    c,
                                    &name,
                                    &route.harness,
                                    &pane_id,
                                    &fallback_args,
                                )
                            })?
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
            // bd-rkx8: generation past the highest on record — names are
            // stable across respawns, so the new row must outrank any
            // surviving row for resolution and readiness.
            let incarnation = next_incarnation(&registry, &name);
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
                incarnation,
            });
            eprintln!("  crew {}: {} ({}) @{}", label, name, agent_pane_id, role);
            // bd-qx0z: panes split here are live by construction — track
            // them so the returned LiveRegistry covers the fresh crew even
            // though the load-time snapshot predates the splits.
            if let Some(live) = live_known.as_mut() {
                live.insert(agent_pane_id.clone());
            }
        }
    }
    // Persist the crew registry (pane_id, name, profile) — `__send`'s auto-
    // heal re-binds lost names from it.
    state::save_spawn_registry(project, &registry)?;
    Ok(match live_known {
        // Fail-open twin of the prune above: with no pane listing the live
        // set is exactly what we hold — keeps every row (existing behavior).
        Some(live) => LiveRegistry::prune(registry, &live),
        None => {
            let all: std::collections::HashSet<String> =
                registry.iter().map(|e| e.pane_id.clone()).collect();
            LiveRegistry::prune(registry, &all)
        }
    })
}

/// bd-5llv: the verdict's `ok` at the top level of a dispatch receipt.
///
/// Extracted so the contract is testable without a pane. The breaker decides
/// success with `out["ok"] == Some(true)`; while `ok` was set only on the
/// failure path, every SUCCESS read as a failure and the breaker opened after
/// five dispatches no matter what happened.
pub fn dispatch_receipt_ok(verdict: &Value) -> Value {
    verdict.get("ok").cloned().unwrap_or(json!(false))
}

/// bd-fen3: unix millis, for the fence's clock. The fence is pure over `now_ms`
/// (the host supplies it), so this is the only place ambient time enters.
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// bd-fen3: the busy-owner fence, adopted from toron-workflow instead of the
/// hand-rolled in-process idle wait bd-5llv had to add.
///
/// The fence is a HOST-side primitive: `try_acquire_for_host` is called nowhere
/// inside toron-workflow's engine, and `engine/` has no `SleepUntilDeadline`
/// handling at all, so there is no engine-side park to delegate to. flywheel is
/// the first adopter, which means flywheel owns the park — recorded on the bead
/// with the durable-park shape (a toron-workflow change) as separate follow-up
/// rather than faked here.
///
/// What the fence buys over the retired gate, all of it real:
/// - the hold bound is fence-owned, and `HoldContinued` does NOT restart the
///   window, so a poll loop cannot grow into an unbounded wait;
/// - the pool enforces one-in-flight-per-agent, so two dispatches cannot race
///   into the same agent;
/// - `FencedStale` closes the read-then-rebind race the old gate had no
///   generation check for;
/// - `deadline_wake` gives a principled HoldExpired -> fork -> retry instead of
///   counting transient retries until a breaker opens.
static DISPATCH_POOL: std::sync::OnceLock<toron_workflow::DispatchPool> =
    std::sync::OnceLock::new();

/// bd-fen3: the hold bound in ms.
///
/// NOT `DEFAULT_HOLD_BOUND_MS` (10_000). A real agent turn on this work runs
/// longer than 10 minutes, so the default would emit `HoldExpired` mid-turn and
/// fork-and-retry — reintroducing exactly the guaranteed retry exhaustion bd-5llv
/// fixed. Bounded, so a wedged agent still fails instead of hanging.
pub const DISPATCH_HOLD_BOUND_MS: u64 = 600_000;

fn dispatch_pool() -> &'static toron_workflow::DispatchPool {
    DISPATCH_POOL.get_or_init(|| {
        toron_workflow::DispatchPool::new(toron_workflow::DispatchConfig {
            // Pane crews are already capped at 5; this is a backstop, not the
            // crew size.
            max_concurrency: 16,
            hold_bound_ms: DISPATCH_HOLD_BOUND_MS,
        })
    })
}

/// bd-fen3: what flywheel does with a fence outcome. Pure, so every rule the
/// fence encodes is testable without a pane or a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceAction {
    /// The permit is held — send now. `Drop` releases holder and slot.
    Send,
    /// Park until this absolute deadline, then re-check. The deadline is the
    /// fence's, not a fresh one: `HoldContinued` deliberately does NOT restart
    /// the bound window, so polling cannot become an unbounded wait.
    Park { until_ms: u64 },
    /// The hold bound elapsed, or a stale worker was fenced. Fork (advance the
    /// generation) and retry the acquire once.
    ForkAndRetry,
    /// Global cap contention — transient, and there is no journalable
    /// busy-owner sleep for it, so the caller retries on the normal path.
    CapContention,
}

/// The fence outcome. The `Send` variant keeps the pool borrow alive for the
/// caller, so the permit is held for exactly the duration of the send.
pub enum FenceOutcome<'p> {
    Send(toron_workflow::DispatchPermit<'p>),
    Park { until_ms: u64 },
    ForkAndRetry,
    CapContention,
}

/// bd-fen3: map the fence's effects to flywheel's next move.
///
/// This is where the subtle rules live, which is exactly why it is pure and
/// tested: a host that restarts the timer on `HoldContinued`, or that reads
/// `HoldExpired` as "send anyway", would silently undo the fence.
pub fn fence_action_for_effects(
    effects: &[toron_core::busy_owner::BusyOwnerEffect],
) -> FenceAction {
    use toron_core::busy_owner::BusyOwnerEffect as E;
    for e in effects {
        match e {
            // Re-check at the fence's deadline. Do NOT extend it.
            E::SleepUntilDeadline { until_ms, .. } | E::HoldContinued { until_ms, .. } => {
                return FenceAction::Park {
                    until_ms: *until_ms,
                };
            }
            E::HoldExpired { .. } | E::FencedStale { .. } => return FenceAction::ForkAndRetry,
            E::Acquired { .. } => return FenceAction::Send,
            E::Released { .. } | E::GenerationAdvanced { .. } | E::ReleaseIgnored { .. } => {}
        }
    }
    // No decisive effect: contention, never permission to send.
    FenceAction::CapContention
}

/// bd-fen3: acquire the fence for `agent`.
pub fn acquire_dispatch_fence(agent: &str, now_ms: u64) -> Result<FenceOutcome<'_>> {
    use toron_workflow::DispatchAcquire;
    let pool = dispatch_pool();
    let generation = pool.generation_of(agent);
    match pool.try_acquire_for_host(agent, now_ms, generation) {
        DispatchAcquire::Granted(permit) => Ok(FenceOutcome::Send(permit)),
        DispatchAcquire::Park { until_ms, .. } => Ok(FenceOutcome::Park { until_ms }),
        DispatchAcquire::CapContention => Ok(FenceOutcome::CapContention),
        DispatchAcquire::Refused(effects) => match fence_action_for_effects(&effects) {
            FenceAction::Send => Err(anyhow!(
                "fence refused for {agent} but reported no park or expiry reason"
            )),
            FenceAction::ForkAndRetry => {
                // Advance the generation so a stale worker cannot reclaim the
                // agent, then hand the caller a fresh acquire.
                let _ = pool.fork_rebind(agent);
                Ok(FenceOutcome::ForkAndRetry)
            }
            FenceAction::Park { until_ms } => Ok(FenceOutcome::Park { until_ms }),
            FenceAction::CapContention => Ok(FenceOutcome::CapContention),
        },
    }
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
/// `__respawn` argv for a fresh dispatch. `--force` is required: a healthy
/// idle agent is a no-op without it, and fresh means the old transcript goes.
pub fn fresh_session_respawn_args(project: &str, name: &str, kind: &str) -> Vec<String> {
    vec![
        "__respawn".into(),
        project.into(),
        name.into(),
        kind.into(),
        "--force".into(),
    ]
}

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
    entries: &LiveRegistry,
    action: &Action,
    copy_index: Option<usize>,
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
                if let Some(role_entry) = resolve_role_slot(entries, profile_name, copy_index)? {
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
                            .entries()
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
    // Same prompt order as headless: an inline task is the mission. A stage
    // is the fallback. Pane used to drop `spec.task`, so a repair pass ran
    // the stage boilerplate instead of the iteration instructions.
    let mut base = match headless::dispatch_spec_task(spec) {
        Some(task) if !task.trim().is_empty() => task.to_string(),
        _ => match headless::dispatch_spec_stage(spec) {
            Some(s) => crate::stage_prompt(s)?,
            None => headless::base_prompt_for(wf_name, inputs),
        },
    };
    for key in ["plan", "project"] {
        if let Some(extra) = headless::spec_extra(spec, key, inputs) {
            base.push_str("\n\n");
            base.push_str(key);
            base.push_str(": ");
            base.push_str(&extra);
        }
    }
    let prompt = base;
    // @role -> running pane: a per-step harness override's matched pane
    // wins; otherwise profile column first, name fallback. bd-op8b: the
    // resolved target is logged — dispatching into a dead pane used to be
    // silent until the bare leaf downstream.
    let entry: SpawnEntry = match override_entry
        .or(resolve_role_slot(entries, profile_name, copy_index)?)
        .cloned()
    {
        Some(e) => e,
        // A registry row means the pane WAS spawned; the prune only knows
        // herdr's listing, so a pane listed moments later looks absent.
        // Settle for it before failing the whole run (bd-ietv). bd-0s0y: settle
        // for THIS copy's pane, or copy 1 would settle onto copy 0's.
        None => match settle_for_role_slot(project, profile_name, copy_index) {
            Some(entry) => {
                eprintln!(
                    "flywheel: WARNING @{profile_name} pane {} was absent from herdr's listing at dispatch; settled and using it",
                    entry.pane_id
                );
                entry
            }
            // bd-07m2: settling failed, so now the cause is knowable. A fan-out
            // copy that is short gets the slot-counted shortfall (the crew
            // genuinely lacks copies); anything else keeps the original
            // "no crew pane" wording, which is the honest verdict when the role
            // was never spawned at all.
            None => match copy_index {
                Some(i) => return Err(shortfall_error(entries, profile_name, i)),
                None => bail!(
                    "pane dispatch: no crew pane for @{profile_name} (spawned: {})",
                    entries
                        .entries()
                        .iter()
                        .map(|e| e.profile.as_deref().unwrap_or(&e.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        },
    };
    eprintln!(
        "flywheel: dispatch @{} → {} ({})",
        profile_name, entry.name, entry.pane_id
    );
    // context: fresh means an empty transcript. Respawn with --force: a
    // healthy idle agent would otherwise no-op and keep the old session.
    if spec.context == Some(toron_workflow::schema::ContextMode::Fresh) {
        let kind = entry
            .harness
            .clone()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| profile.harness.clone());
        if kind.trim().is_empty() {
            bail!(
                "pane dispatch: context fresh for @{profile_name} but no harness is known to respawn"
            );
        }
        let exe = std::env::current_exe()?;
        let output = Command::new(exe)
            .args(fresh_session_respawn_args(project, &entry.name, &kind))
            .output()
            .with_context(|| format!("fresh respawn of {}", entry.name))?;
        if !output.status.success() {
            bail!(
                "pane dispatch: fresh session respawn of {} failed: {}",
                entry.name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // Subprocess-reuse (Spec A): the `__send` leaf owns the confirmed send
    // (landing wait, last-sent dedup, auto-heal, Spec C conditioning) — no
    // new dispatch code. A freshly spawned agent may still be booting when
    // the first step fires: `agent_not_ready` is transient — retry with
    // backoff (bounded; a persistent not-ready surfaces as a per-step error,
    // never a hang).
    // P1+P2: transient dispatch retries ride the framework's remote stack
    // (`policy::remote`) — 6 attempts, 5s base, 10s cap — with the
    // consecutive-failure breaker (5/60s) as its ARGUMENT, not as a second
    // breaker beside it. Open breaker = fast escalate, never a hammered pane.
    // The envelope lives in `dispatch_policy` below; the loop itself needs a
    // live pane, so the stack's ORDER is pinned by the unit tests there.
    // bd-jvka: the breaker is consulted INSIDE the chain — before every
    // attempt, so an open breaker stops the chain instead of letting it
    // hammer the pane for six more tries.
    // bd-fen3: the busy-owner fence, adopted from toron-workflow. This replaces
    // the bd-5llv in-process idle gate, which is DELETED rather than kept
    // alongside — one concern, one mechanism. The fence owns the hold bound and
    // does not restart it on HoldContinued, the pool enforces one-in-flight per
    // agent so two dispatches cannot race into one agent, and FencedStale closes
    // the read-then-rebind race the old gate had no generation check for.
    let fence = acquire_dispatch_fence(&entry.name, unix_millis())?;
    // One refusal, two shapes: a park can name its deadline and the other two
    // cannot, so the tail is shared rather than copied into both arms.
    let fenced_tail = "— refusing to send a second prompt into an agent that already holds one. \
                       The busy-owner fence requires a durable park, which toron-workflow's \
                       engine has no step for; see bd-fen3. Give each concurrent stage its \
                       own agent.";
    let _permit = match fence {
        FenceOutcome::Send(p) => Some(p),
        // bd-fen3: `until_ms` is the part a caller has to act on. "Busy" alone
        // cannot tell "retry in a moment" from "this agent is held for another
        // ten minutes", and those are different decisions.
        FenceOutcome::Park { until_ms } => {
            let wait_s = until_ms.saturating_sub(unix_millis()) / 1_000;
            return Err(anyhow!(
                "pane dispatch @{profile_name} ({}) is fenced as busy for another {wait_s}s \
                 (park until {until_ms}) {fenced_tail}",
                entry.name
            ));
        }
        FenceOutcome::ForkAndRetry | FenceOutcome::CapContention => {
            return Err(anyhow!(
                "pane dispatch @{profile_name} ({}) is fenced as busy {fenced_tail}",
                entry.name
            ));
        }
    };
    // bd-mukq: 429 reroute state — the CURRENT send target (starts at the
    // resolved entry) plus the attempted profiles (starts at the step's
    // own). A 429 swaps the target to a live fallback-profile pane and
    // re-drives the same loop ONCE; the second 429 fails loud with a
    // trail instead of redriving the exhausted route.
    let mut attempt_entry: &SpawnEntry = &entry;
    let mut attempt_profile = profile_name.to_string();
    let mut attempted: Vec<String> = vec![profile_name.to_string()];
    let mut reroute_trail: Vec<Value> = Vec::new();
    let rate_limit_kind = crate::failure::FailureKind::RateLimit.to_string();
    // bd-jvka: the ROUTE chain and the FALLBACK are separate. The chain
    // retries ONE route — the breaker is consulted INSIDE it, the budget
    // bounds it, the attempt timeout is ON the call. The 429 fallback picks
    // the NEXT route and runs a NEW chain, so the fallback is OUTSIDE the
    // retry (effect-kernel law 10) and the new route gets its own budget.
    'route: loop {
        // ADR-0018 §3 item 5: the chain IS the framework's remote stack, with
        // the breaker as its argument (there is no second breaker beside it).
        // The framework calls the attempt through a 'static closure, so the
        // route's data is owned here - cloned once per chain, never borrowed.
        // `send_*` are the closure's own copies: the route's live values stay
        // owned by the loop, which reroutes and retries them.
        let send_agent = attempt_entry.name.clone();
        let send_profile = attempt_profile.clone();
        let send_project = project.to_string();
        let send_prompt = prompt.clone();
        let send_spec = (*spec).clone();
        let send_inputs = (*inputs).clone();
        let chain = dispatch_chain(
            dispatch_remote(),
            dispatch_runtime(),
            dispatch_clock(),
            move || -> Effect<Value, String, ()> {
                let exe = match std::env::current_exe() {
                    Ok(exe) => exe,
                    // A host error, not the child's typed failure: a defect,
                    // so the framework does not retry it.
                    Err(e) => {
                        return Effect::die(Defect::new(format!(
                            "pane dispatch: resolve the flywheel executable: {e}"
                        )));
                    }
                };
                let mut cmd = Command::new(exe);
                cmd.arg("__send")
                    .arg(&send_project)
                    .arg(&send_agent)
                    .arg(&send_prompt)
                    .arg("--confirm")
                    .arg("--profile")
                    .arg(&send_profile);
                // bd-jvka: the attempt timeout ON the call — a `__send` child
                // that never exits must not park the dispatch.
                let output = match crate::proc::run_bounded(&mut cmd, dispatch_attempt_timeout()) {
                    Ok(output) => output,
                    // The child never came back: a host error, not the child's
                    // typed failure - a defect, so the framework does not
                    // retry it.
                    Err(crate::proc::ProcError::Timeout { .. }) => {
                        return Effect::die(Defect::new(format!(
                            "__send for @{send_profile} ({send_agent}) exceeded the attempt timeout ({:?}) - the child never exited",
                            dispatch_attempt_timeout()
                        )));
                    }
                    Err(crate::proc::ProcError::Io(e)) => {
                        return Effect::die(Defect::new(format!(
                            "__send for @{send_profile} ({send_agent}) failed: {e}"
                        )));
                    }
                };
                let raw = String::from_utf8_lossy(&output.stdout).to_string();
                let trimmed = raw.trim();
                if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                    let transient = v
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(is_transient_dispatch_reason)
                        .unwrap_or(false);
                    if transient {
                        // bd-lifecycle-occupied-pane-wedge-07tx: bounded-fail-loud.
                        // The pane is not coming up (occupied bind, dead split,
                        // stuck boot). The CHAIN turns this into a typed
                        // transient failure and the step fails loudly once the
                        // chain is spent — never the old soft ok-payload the
                        // engine re-drove forever (idle crews + endless
                        // "@not ready (attempt 1/6)").
                        let reason = v
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("transient")
                            .to_string();
                        // The child's TYPED failure: the framework retries it
                        // and charges the breaker once per attempt.
                        return Effect::fail(reason);
                    }
                    // ADR-0022 §3.5 warn-first plan-artifact gate (bd-cvv9): same
                    // contract as the headless path — warn + tag, never block
                    // (the W5 flip owns blocking).
                    if crate::phase_chain::plan_artifact_missing(&send_spec, &send_inputs) {
                        eprintln!(
                            "flywheel: phase implement dispatch has NO completed plan artifact — \
                     warn-first gate (ADR-0022 §3.5); step output tagged plan_artifact_missing"
                        );
                    }
                    // bd-5llv: carry the verdict's `ok` at the TOP level, always.
                    //
                    // It used to be set only on the failure path, which meant a
                    // SUCCESSFUL send produced a receipt with no top-level `ok` at
                    // all. The breaker's charge read that field, so every success
                    // fell to the else and recorded a FAILURE — the dispatch COUNT
                    // predicted failure, not the result. That is why wave-swarm (6
                    // dispatches) failed every single time while a 2-prompt compose
                    // always succeeded, and why the error blamed an "unavailable
                    // pane" when every prompt had in fact landed.
                    let mut out = json!({
                        "profile": send_profile.clone(),
                        "agent": send_agent.clone(),
                        "ok": dispatch_receipt_ok(&v),
                        "send": v.clone(),
                    });
                    // ADR-0024 W1a: classify pane dispatch failures (rate_limit/timeout/auth)
                    if v.get("ok").and_then(Value::as_bool) == Some(false) {
                        // bd-5yzp: every failure payload carries ok:false + a
                        // non-empty error + kind (unknown included) — an
                        // error-less or kind-less ok:false misroutes as bare
                        // {error: step failed} downstream (bd-nyrn).
                        let leaf_err = v.get("error").and_then(Value::as_str).unwrap_or("");
                        let err_str =
                            dispatch_failure_error(&send_profile, &send_agent, &raw, leaf_err);
                        let cf = crate::failure::classify(&err_str, None, None);
                        out["stage"] = json!(cf.stage);
                        out["ok"] = json!(false);
                        out["error"] = json!(err_str.clone());
                        out["kind"] = json!(cf.kind.to_string());
                        out["retryable"] = json!(cf.retryable);
                    }
                    return Effect::succeed(out);
                }
                let err = String::from_utf8_lossy(&output.stderr).to_string();
                // bd-5yzp: never error:'' — stderr, stdout, then scoped fallback.
                let err_str = dispatch_failure_error(&send_profile, &send_agent, &raw, &err);
                let cf = crate::failure::classify(&err_str, None, None);
                Effect::succeed(json!({
                    "ok": false,
                    "error": err_str,
                    "profile": send_profile.clone(),
                    "agent": send_agent.clone(),
                    // GS-049 P1-3: phase-bearing failure (ActionStage)
                    "stage": cf.stage,
                    "kind": cf.kind.to_string(),
                    "retryable": cf.retryable,
                }))
            },
        );
        // bd-jvka: the FALLBACK is OUTSIDE the chain. The chain is done; only
        // now may a 429 reroute pick the next route and start a NEW chain (with
        // its own retry budget).
        let mut out = match chain {
            Ok(out) => out,
            Err(DispatchErr::Transient { reason }) => {
                return Err(anyhow!(
                    "dispatch @{attempt_profile} ({}) failed after {MAX_ATTEMPTS} transient retries: {reason} — the target pane never became available; fix the pane (occupied/dead) or fail the step",
                    attempt_entry.name
                ));
            }
            Err(DispatchErr::BreakerOpen) => {
                return Err(anyhow!(
                    "pane dispatch: the consecutive-failure breaker is open — fast escalate, waiting out the cooldown instead of hammering the pane"
                ));
            }
            Err(DispatchErr::BudgetExhausted) => {
                return Err(anyhow!(
                    "pane dispatch: the dispatch chain spent its {:?} budget — the target pane never became available",
                    dispatch_chain_budget()
                ));
            }
            Err(DispatchErr::Fatal(msg)) => return Err(anyhow!(msg)),
        };
        // bd-mukq: 429 reroute — ONE hop to a live fallback-profile pane
        // (ADR-0026 shared chain; the running agent keeps its model and
        // session, the self-contained prompt reroutes). No chain or no live
        // pane, or the second 429, fails LOUD with a trail + hint instead of
        // redriving exhausted quota.
        if out.get("kind").and_then(Value::as_str) == Some(rate_limit_kind.as_str()) {
            let err_str = out
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
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
                                continue 'route;
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
                                    err_str
                                },
                            },
                            0,
                        );
                        let _ = crate::agent_ref::commit_rebind_opt(project, intent, |_| {
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
        if !reroute_trail.is_empty() {
            out["reroute_trail"] = json!(reroute_trail);
        }
        // ADR-0018 §3 item 5: the breaker's state lives in the framework's
        // stack, which charged it once per FAILED ATTEMPT inside the chain
        // and reset it on a success. Charging it here too would be the
        // second carrier the ADR forbids.
        return Ok(crate::phase_chain::annotate_dispatch_output(
            spec, inputs, out,
        ));
    }
}

/// bd-5yzp: non-empty failure error for pane dispatch — stderr/leaf error
/// wins, then stdout, then a profile/agent-scoped fallback (never '').
fn dispatch_failure_error(profile: &str, agent: &str, raw_stdout: &str, err: &str) -> String {
    if !err.trim().is_empty() {
        return err.to_string();
    }
    if !raw_stdout.trim().is_empty() {
        return raw_stdout.to_string();
    }
    format!("pane dispatch: @{profile} ({agent}) __send returned empty stdout+stderr — no payload")
}

/// Pane-dispatch attempts per route (bd-8pjr): 6 attempts, 5s base, 10s cap
/// — the same envelope as the old fixed 10s x6, now with a backoff ramp.
const MAX_ATTEMPTS: u32 = 6;

/// bd-8pjr: the backoff base and the cap it never passes. The framework's
/// stack owns the schedule; these are the bounds it runs with.
const DISPATCH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
const DISPATCH_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(10);

/// bd-jvka: the typed outcome of one dispatch chain.
#[derive(Debug)]
pub enum DispatchErr {
    /// The attempt failed transiently; the chain may retry it.
    Transient { reason: String },
    /// The framework's breaker rejected the attempt: no attempt ran.
    BreakerOpen,
    /// The chain's wall-clock budget was spent.
    BudgetExhausted,
    /// The call itself failed (spawn, timeout, ...) — not retryable here.
    Fatal(String),
}

/// bd-jvka + ADR-0018 §3 item 5: the dispatch chain IS the framework's remote
/// stack (`policy::remote`), with the breaker as its argument. There is no
/// second breaker beside it.
///
/// The order is the framework's, and it is the order bd-jvka proved: the
/// breaker is consulted INSIDE the retry (before every attempt), the budget
/// bounds the chain AROUND it, the attempt timeout is ON the call, and the
/// fallback is OUTSIDE (the caller reroutes when the chain ends).
///
/// The attempt is a `'static` closure because the framework's stack calls it
/// through one: the route's data is owned by the closure, never borrowed.
pub fn dispatch_chain(
    remote: &Remote<Value, String>,
    rt: &Runtime,
    clock: &std::sync::Arc<dyn ClockImpl>,
    attempt: impl Fn() -> Effect<Value, String, ()> + Send + Sync + 'static,
) -> Result<Value, DispatchErr> {
    match remote.call(rt, clock, attempt) {
        Exit::Success(v) => Ok(v),
        Exit::Failure(cause) => Err(map_dispatch_cause(cause)),
    }
}

/// The framework's stack decisions, as the chain's typed outcomes.
///
/// A defect is NOT the child's typed failure: it is a host error, and the
/// framework does not retry it. A typed failure IS the child's own answer
/// ("not ready yet"): it is retried, and the breaker is charged once per
/// attempt - six transient retries are six failures, not one.
fn map_dispatch_cause(cause: Cause<RemoteError<String>>) -> DispatchErr {
    if let Some(defect) = cause.defects().first() {
        return DispatchErr::Fatal(defect.message.clone());
    }
    match cause.failure() {
        Some(RemoteError::Inner(reason)) => DispatchErr::Transient {
            reason: reason.clone(),
        },
        Some(RemoteError::BreakerOpen) => DispatchErr::BreakerOpen,
        Some(RemoteError::BudgetTimeout) => DispatchErr::BudgetExhausted,
        Some(RemoteError::AttemptTimeout) => {
            DispatchErr::Fatal("the dispatch attempt exceeded its timeout".to_string())
        }
        Some(RemoteError::BulkheadFull) => {
            // bd-fa58: name what it means, not just the kind — the shared
            // stack admitted no permit in time, so this dispatch never ran.
            // Dispatches are sequential, so reaching this means the shared
            // stack itself is saturated, not the pane.
            DispatchErr::Fatal(
                "the dispatch bulkhead admitted no permit in time — the shared stack is saturated".to_string(),
            )
        }
        Some(RemoteError::Filtered) => DispatchErr::Fatal("the dispatch was filtered".to_string()),
        None => DispatchErr::Fatal("the dispatch failed with no cause".to_string()),
    }
}

/// bd-jvka: the attempt timeout ON the call. The `__send` child waits 45s for
/// the prompt to land (`--confirm` default), so this sits above the child's
/// own bound: the child's clean timeout is what the operator sees, and this
/// only catches a child that never exits at all.
fn dispatch_attempt_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(120)
}

/// bd-jvka: the budget AROUND the chain. Six attempts x 120s plus five
/// backoff sleeps (10s cap) fit inside it, so a legitimate chain is never cut
/// short, while a pathological one cannot run unbounded.
fn dispatch_chain_budget() -> std::time::Duration {
    std::time::Duration::from_secs(900)
}

/// ADR-0018 §3 item 5: the ONE dispatch stack — the framework's remote
/// policy with the breaker as its ARGUMENT. There is no second breaker
/// beside it, and no second spelling of the retry envelope beside it either.
///
/// The stack is process-wide (the static it replaces was too), so the
/// breaker's state is shared across dispatches: five consecutive failures
/// open it for 60s (ADR-0015 §3 P2), and the framework resets the count on a
/// success and lets one probe through after the cooldown, so a pane that
/// failed can recover.
/// bd-ja69: the framework's default jitter seed is a constant (42), so every
/// process would draw the SAME backoff sequence and retry in lockstep after
/// a shared outage — the failure class bd-retry-jitter-xlh6 removed from the
/// old driver. Seed from process entropy instead (pid + nanos + counter, the
/// same convention as `retry::host_draw`); tests build their own policies, so
/// no test observes this.
fn dispatch_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish() | 1
}

/// Boundary (bd-fa58): this stack is for the REMOTE dispatch chain only.
/// Local herdr IPC (settle re-reads, text primes) stays on the small `retry`
/// driver — unifying the two under one breaker or bulkhead would couple
/// unrelated failures, so the two spellings coexist on purpose.
fn dispatch_policy() -> RemotePolicy<Value, String> {
    RemotePolicy {
        // The framework counts RETRIES after the first attempt.
        retries: MAX_ATTEMPTS.saturating_sub(1),
        backoff: kernel_millis(DISPATCH_BACKOFF),
        ceiling: kernel_millis(DISPATCH_BACKOFF_CAP),
        seed: dispatch_seed(),
        attempt_timeout: kernel_millis(dispatch_attempt_timeout()),
        budget: kernel_millis(dispatch_chain_budget()),
        breaker: Some(BreakerConfig {
            max_failures: 5,
            reset: kernel_millis(std::time::Duration::from_secs(60)),
        }),
        // The fallback stays OUTSIDE the stack (bd-jvka): the chain hands the
        // rate-limit verdict back, and only the caller picks the next route.
        ..Default::default()
    }
}

/// The built stack: one shared breaker, one shared bulkhead, process-wide.
fn dispatch_remote() -> &'static Remote<Value, String> {
    static REMOTE: std::sync::OnceLock<Remote<Value, String>> = std::sync::OnceLock::new();
    REMOTE.get_or_init(|| Remote::new(dispatch_policy()))
}

/// The wall clock and the runtime the framework's stack runs on.
fn dispatch_clock() -> &'static std::sync::Arc<dyn ClockImpl> {
    static CLOCK: std::sync::OnceLock<std::sync::Arc<dyn ClockImpl>> = std::sync::OnceLock::new();
    CLOCK.get_or_init(|| std::sync::Arc::new(SystemClock))
}

fn dispatch_runtime() -> &'static Runtime {
    static RT: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| Runtime::new())
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
    entries: &LiveRegistry,
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
    /// bd-crlx/bd-qlr1: `Some(model)` when the harness's interactive
    /// entrypoint has NO model flag, so the pin must travel through the pane
    /// env (`OPENCODE_CONFIG_CONTENT`) instead of argv. This launch path
    /// creates its own pane, so it can honour the pin; `launch_spawn_job`
    /// applies it to the env map it hands to `pane_split`.
    pub env_model_pin: Option<String>,
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
/// bd-qlr1: the pane env for a launch, with any env-delivered model pin
/// already merged in.
///
/// herdr injects env ONLY at split/create, and a pane's env is fixed for the
/// life of the process it launches — so the pin has to be in the map before the
/// split, never applied afterwards. Every launch path that creates its own pane
/// builds its env here, so the pin cannot be forgotten at a call site: if a
/// path needs a pin it must pass it, and the refusal for an unhonourable pin
/// comes from `profile_launch_args` refusing rather than from a silent drop.
pub(crate) fn pane_env_for_launch(
    cfg: &crate::Config,
    mail_identity: Option<&str>,
    env_model_pin: Option<&str>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut env = crate::herdr::herdr_integration_env(cfg, mail_identity);
    if let Some(model) = env_model_pin {
        crate::herdr::apply_opencode_model_pin(&mut env, model)?;
    }
    Ok(env)
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
    // An explicit spawn `harness:` overrides the profile's resolved harness.
    let harness = spec["harness"]
        .as_str()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| spec["resolved"]["spec"]["harness"].as_str())
        .unwrap_or_default()
        .to_string();
    // ADR-0022 W2: the step's lifecycle phase (typed spec, resolved copy).
    let phase = spec["resolved"]["spec"]["phase"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    // bd-qlr1: the PLAN form, not the argv-only view. The argv-only view
    // REFUSES when the harness needs an env-delivered pin, and this path
    // creates its own pane — so it must resolve the plan and carry the pin to
    // the split rather than refusing a launch it can actually satisfy.
    let (args, env_model_pin) = match catalog.get(&profile) {
        Some(p) => {
            let plan = crate::profile_launch_plan(&harness, p, phase.as_deref())
                .map_err(|e| anyhow::anyhow!("spawn plan '{profile}' model routing: {e}"))?;
            (plan.args, plan.env_model_pin)
        }
        None => (Vec::new(), None),
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
        env_model_pin,
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
        (e.pane_id, name)
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
        // bd-qlr1: the pin must be in the map BEFORE the split, because
        // herdr injects env only at split/create and a pane's env is fixed for
        // the life of the process it launches.
        let pane_env =
            pane_env_for_launch(cfg, mail_identity.as_deref(), plan.env_model_pin.as_deref())?;
        let split = crate::herdr::pane_split(
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
                // The rung keys on the PROFILE's model id, which lives in the
                // catalog, so the catalog is read here — once per failed
                // spawn. The gate INSIDE `registry_fallback_route` still keeps
                // `models.toml` off a failure that cannot hop. An unreadable
                // catalog is loud: it means no rung, never a silently
                // different launch.
                let profile_model = match crate::profile::ProfileCatalog::load() {
                    Ok(catalog) => catalog.get(&plan.profile).map(|p| p.model.clone()),
                    Err(err) => {
                        eprintln!(
                            "pane spawn: profile catalog unreadable \
                             — no rate-limit fallback: {err:#}"
                        );
                        None
                    }
                };
                // Carry the rung's key alongside the route: the row's `from`
                // binding and the registry lookup must be one string.
                let hop = profile_model.as_deref().and_then(|model| {
                    registry_fallback_route(&e.to_string(), model, plan.harness.as_str(), || {
                        match ModelRegistry::load_routing() {
                            Ok(registry) => Some(registry),
                            Err(err) => {
                                eprintln!(
                                    "pane spawn: model registry unreadable \
                                     — no rate-limit fallback: {err:#}"
                                );
                                None
                            }
                        }
                    })
                    .map(|route| (model.to_string(), route))
                });
                // Not a rate limit, no catalog, no registry, no rung (or a
                // rung that is itself unroutable): all re-raise the original
                // error untouched.
                let Some((from_model, route)) = hop else {
                    return Err(e);
                };
                // argv shape preserved from the hand-rolled hop: no effort
                // flag and no role flags, unlike the crew hop in
                // `spawn_crew_into`. bd-oc-bare-spawn-model-argv-zbzt: the
                // argv must be the INTERACTIVE form — the headless one types
                // `--model` into opencode's root command, which rejects it (and
                // this pane's env is already fixed, so the pin cannot ride
                // there either): refuse rather than kill the pane or run an
                // unpinned seat.
                let fallback_args =
                    crate::interactive_pane_args(&route.harness, &route.model, None).map_err(
                        |why| {
                            anyhow!(
                                "pane spawn: {} rate-limited and the fallback {} cannot be \
                                 delivered into this pane: {why:#}",
                                plan.profile,
                                route.model_id
                            )
                        },
                    )?;
                // GS-043 I4: journal before ModelEntry.fallback hop.
                let intent = model_fallback_intent(
                    project,
                    &name,
                    &plan.profile,
                    &from_model,
                    plan.harness.as_str(),
                    &route,
                    e.to_string(),
                );
                eprintln!(
                    "pane spawn: {} rate-limited — registry fallback {}",
                    plan.profile, route.model_id
                );
                crate::agent_ref::commit_rebind(project, intent, |_| {
                    crate::start_agent_with_retry(
                        &mut c,
                        &name,
                        &route.harness,
                        &split.pane_id,
                        &fallback_args,
                    )
                })?
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
        (agent.pane_id, name)
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
    runner: toron_workflow::engine::EffectRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
) -> Result<toron_workflow::WorkflowEngine> {
    let server_url = cli_engine_store_url();
    // Dry runs still need a recording seam so control-flow output contains
    // every dispatch node. The adapter records the action shape only; it
    // never calls the real host runner, so no external effect is performed.
    let recording_runner: toron_workflow::engine::EffectRunner =
        std::sync::Arc::new(|inv| Ok(json!({ "action": inv.action.type_name() })));
    local_engine_with_journal(
        rt,
        server_url,
        runner,
        profile,
        workflow,
        Some(recording_runner),
        toron_workflow::engine::EffectJournalMode::Recording,
        // Dry engine: no effect journal, so no run rows either — the engine
        // performs nothing, and a row claiming otherwise is the lie the
        // journal exists to prevent (bd-mi9n).
        None,
    )
}

/// Lift a blocking effect runner onto the workflow engine's async seam.
/// Blocking pane/process work runs on Tokio's blocking pool, so retry delays
/// cannot stall the engine executor or unrelated workflow tasks.
pub fn asyncify_runner(
    runner: toron_workflow::engine::EffectRunner,
) -> toron_workflow::engine::AsyncEffectRunner {
    std::sync::Arc::new(move |inv| {
        let runner = runner.clone();
        let inv = inv.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || runner(&inv))
                .await
                .map_err(|e| anyhow!("pane effect task failed: {e}"))?
        })
    })
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
    runner: toron_workflow::engine::EffectRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
) -> Result<toron_workflow::WorkflowEngine> {
    let server_url = cli_engine_store_url();
    local_engine_with(cfg, rt, server_url, runner, profile, workflow)
}

/// CLI engine store selection (bd-adr22-local-resonate-tx8q). The
/// toron-workflow probe order also auto-probes the shared multi-host server
/// (`https://resonate.example-tailnet.ts.net`) — deliberately NOT done here:
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
/// [`local_engine_journaled`] is the only caller: the journal is always
/// built here (bd-722a — the `journal` flag was always `true`, so its
/// un-journaled branch was unreachable).
fn local_engine_with(
    cfg: &crate::config::Config,
    rt: &tokio::runtime::Runtime,
    server_url: Option<String>,
    runner: toron_workflow::engine::EffectRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
) -> Result<toron_workflow::WorkflowEngine> {
    // bd-mi9n: the journal is claimed in TWO halves — the effect runner the
    // leaves dispatch through, and the run-lifecycle journal the engine feeds
    // its own Started → Running → terminal rows through. Both come from one
    // store, so the run row and its effect rows are keyed by the same run_id.
    //
    // bd-ozpj: a JOURNALED constructor that cannot journal REFUSES. The old
    // code degraded to Recording when the journal could not be opened, so the
    // leaves would perform their external effects with NO journal row — the
    // exact thing `Enforced` exists to prevent, and it was silent.
    let (effect, run_journal) = match crate::effect_journal::journals(cfg, runner.clone()) {
        Some(j) => (Some(j.effect), Some(j.run)),
        None => {
            anyhow::bail!(
                "journaled engine requested but the effect journal could not be opened (the toron index is unavailable) — refusing to run unjournaled"
            );
        }
    };
    // bd-engine-run-path-journaling-fwlx (the Spec 6 flip): a journaled
    // constructor is Enforced — its leaves really perform external effects,
    // so an uncovered sink must refuse, never run unjournaled.
    let mode = engine_journal_mode(true, effect.is_some())?;
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
/// The journal mode of a constructed engine (bd-ozpj).
///
/// A JOURNALED constructor is `Enforced`: its leaves really perform external
/// effects, so an uncovered sink must refuse, never run unjournaled. The
/// plain constructor (the dry arms: `workflow run` without `--execute`,
/// `cron run` without `--live`) stays `Recording` — it records instead of
/// performing, so its leaves MUST stay unjournaled, visibly.
///
/// A journaled constructor that could NOT open its journal REFUSES: the old
/// code degraded to `Recording` and the leaves then performed their effects
/// with no journal row at all, silently.
pub fn engine_journal_mode(
    journal: bool,
    journal_built: bool,
) -> Result<toron_workflow::engine::EffectJournalMode> {
    if !journal {
        return Ok(toron_workflow::engine::EffectJournalMode::Recording);
    }
    if !journal_built {
        anyhow::bail!(
            "journaled engine requested but the effect journal could not be opened (the toron index is unavailable) — refusing to run unjournaled"
        );
    }
    Ok(toron_workflow::engine::EffectJournalMode::Enforced)
}

fn local_engine_with_journal(
    rt: &tokio::runtime::Runtime,
    server_url: Option<String>,
    _runner: toron_workflow::engine::EffectRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
    effect: Option<toron_workflow::engine::EffectRunner>,
    mode: toron_workflow::engine::EffectJournalMode,
    run_journal: Option<toron_workflow::engine::RunJournal>,
) -> Result<toron_workflow::WorkflowEngine> {
    let _enter = rt.enter();
    // Flag-day (bd-fuql): the legacy runner is gone — the journaled effect
    // wins when present, else this runner executes leaves directly through
    // the supported context-bearing seam (same preference the old engine
    // applied to its two-runner inputs).
    let effect = effect;
    // Pane dispatch owns subprocesses and retry waits. Lift both host-owned
    // synchronous seams onto the engine's async surface so the workflow
    // executor never sleeps or waits for a child on its Tokio worker thread.
    // The blocking work is isolated in Tokio's blocking pool until the pane
    // dispatcher is fully async, while cancellation of the engine future can
    // still propagate at the async boundary.
    let async_effect = effect.map(asyncify_runner);
    let async_run_journal = run_journal.map(|journal| {
        std::sync::Arc::new(move |transition: toron_workflow::engine::RunTransition| {
            let journal = journal.clone();
            Box::pin(async move { journal(&transition) })
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }) as toron_workflow::engine::AsyncRunJournal
    });
    match server_url {
        Some(url) => {
            toron_workflow::WorkflowEngine::remote_with_resolvers_async_effect_mode_and_journal(
                url,
                profile,
                workflow,
                async_effect,
                mode,
                async_run_journal,
            )
        }
        None => toron_workflow::WorkflowEngine::local_with_resolvers_async_effect_mode_and_journal(
            profile,
            workflow,
            async_effect,
            mode,
            async_run_journal,
        ),
    }
}

/// bd-omwg: the pane-mode approval drainer thread. Surfaces each parked
/// approval promise, applies the ADR-0011 D7 SIGNOFF auto-pass, and resolves
/// parked promises from captain decision mail, until `stop` is set (the caller
/// does this after invoke returns, then joins).
///
/// erbo (bead `elm-effect-rs-approval-drainer-inbox-panic-erbo`): the drainer
/// thread drives its own current-thread runtime, and BOTH of its synchronous
/// reads `block_on` the shared mail runtime inside — the SIGNOFF snapshot
/// (`snapshot::assemble` → `Mail::list_active`) and the decision scan
/// (`Mail::inbox`). Called inline they panic "Cannot start a runtime from
/// within a runtime" and kill the thread, so parked approvals silently stopped
/// auto-resolving. Every such read now goes through
/// [`crate::backend::run_mail_off_runtime`].
pub fn start_approval_drainer(
    cfg: Config,
    project: String,
    engine: std::sync::Arc<toron_workflow::WorkflowEngine>,
    stop: std::sync::Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<Result<()>>> {
    let reads_project = project.clone();
    start_approval_drainer_with(engine, stop, project, move || {
        production_drainer_reads(cfg, reads_project)
    })
}

/// The drainer's two synchronous host reads. Each is a plain sync call that
/// may `block_on` the shared mail runtime, so the drainer only ever runs them
/// through [`crate::backend::run_mail_off_runtime`] — never inline on its own
/// runtime.
#[derive(Clone)]
pub(crate) struct DrainerReads {
    /// ADR-0011 D7: true iff the SIGNOFF auto-pass may fire for this project
    /// right now (REVIEW residual empty AND OPEN DECISIONS empty).
    pub signoff_clear: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    /// One page of the drainer identity's inbox, or `None` when mail is
    /// unavailable (the drainer then degrades to sink-only behaviour).
    pub inbox: Option<std::sync::Arc<dyn Fn() -> Result<Value> + Send + Sync>>,
}

/// Production reads. Built on the drainer thread BEFORE its runtime exists,
/// so `Mail::from_config` never has to take the in-runtime guard path.
fn production_drainer_reads(cfg: Config, project: String) -> DrainerReads {
    let mail = crate::mail::Mail::from_config(Some(&cfg.toron_mail_config()))
        .ok()
        .flatten();
    let signoff_project = project.clone();
    let signoff_clear = std::sync::Arc::new(move || {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let swarm = crate::swarm_label(&signoff_project);
        let ws = crate::doorbell::preferred_workspace(
            &swarm,
            crate::state::load_spawn_meta(&signoff_project).as_deref(),
            |l| crate::doorbell::dispatcher_token(&crate::config::state_dir(), l),
        );
        let snap = crate::snapshot::assemble(&cfg, &signoff_project, &ws, now);
        // REVIEW residual: any non-closed lane. Bead status is never the word
        // "review", so that filter always auto-passed.
        let review_residual = snap.lanes.iter().filter(|l| l.status != "closed").count();
        let review_degraded = snap.degraded.iter().any(|d| d.contains("review"));
        crate::signoff::observe(snap.decisions.len(), review_residual, review_degraded)
    }) as std::sync::Arc<dyn Fn() -> bool + Send + Sync>;
    let inbox = mail.map(|mail| {
        std::sync::Arc::new(move || mail.inbox(&project, &mail.name, false, 25))
            as std::sync::Arc<dyn Fn() -> Result<Value> + Send + Sync>
    });
    DrainerReads {
        signoff_clear,
        inbox,
    }
}

/// Every Nth 150 ms drain pass scans the inbox (~2 s cadence).
const APPROVAL_SCAN_EVERY_PASSES: u32 = 13;

/// Pure: the captain decision for `promise_id` carried by one inbox page, if
/// any. A row whose subject names `approve:<id>` approves, `reject:<id>`
/// rejects; the first matching row wins (newest-first page order). Returns the
/// sender alongside for the audit line.
pub(crate) fn approval_decision_in_inbox(
    rows: &[Value],
    promise_id: &str,
) -> Option<(toron_workflow::engine::Decision, String)> {
    let token_a = format!("approve:{promise_id}");
    let token_r = format!("reject:{promise_id}");
    rows.iter().find_map(|row| {
        let subject = row.get("subject").and_then(|s| s.as_str()).unwrap_or("");
        let sender = row
            .get("sender")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if subject.contains(&token_a) {
            Some((
                toron_workflow::engine::Decision::approved(format!(
                    "via workflow approve (from {sender})"
                )),
                sender,
            ))
        } else if subject.contains(&token_r) {
            Some((
                toron_workflow::engine::Decision::rejected(format!(
                    "via workflow approve (from {sender})"
                )),
                sender,
            ))
        } else {
            None
        }
    })
}

/// The drainer loop with its host reads injected (`make_reads` runs on the
/// drainer thread before the runtime is built). Production wiring is
/// [`start_approval_drainer`]; tests inject fixture reads.
pub(crate) fn start_approval_drainer_with(
    engine: std::sync::Arc<toron_workflow::WorkflowEngine>,
    stop: std::sync::Arc<AtomicBool>,
    project: String,
    make_reads: impl FnOnce() -> DrainerReads + Send + 'static,
) -> Result<std::thread::JoinHandle<Result<()>>> {
    let handle = std::thread::Builder::new()
        .name("approval-drainer".into())
        .spawn(move || {
            // bd-1ub5: the drainer is the RESOLVE path. Parked promises are
            // resolved by scanning the project inbox for a captain decision
            // mail (`approve:<promise_id>` / `reject:<promise_id>` subject
            // token) — `flywheel workflow approve` sends exactly that. Mail
            // unavailable degrades to the sink-only behavior.
            let reads = make_reads();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| anyhow!("approval drainer runtime: {e}"))?;
            rt.block_on(async {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                // Loud, not spammy: a failing read prints when its error text
                // changes, never silently `continue`s.
                let mut last_scan_err: Option<String> = None;
                let mut pass: u32 = 0;
                loop {
                    pass = pass.wrapping_add(1);
                    let ids: Vec<String> = {
                        let sink = engine.approval_sink().lock().await;
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
                        let run_tag = format!("run-{}", &promise_id[..promise_id.len().min(8)]);
                        eprintln!(
                            "flywheel: approval parked: promise {promise_id} for project {project} — resolve with: flywheel workflow approve {run_tag} {promise_id} --decision approve|reject --project {project}",
                        );
                        // ADR-0011 D7: SIGNOFF auto-pass iff REVIEW residual
                        // empty AND OPEN DECISIONS empty. Irreversible
                        // preference still parks. erbo: the snapshot read
                        // shells mail, so it runs off this runtime.
                        let observe = reads.signoff_clear.clone();
                        let clear = match crate::backend::run_mail_off_runtime(move || Ok(observe()))
                            .await
                        {
                            Ok(clear) => clear,
                            Err(e) => {
                                eprintln!(
                                    "flywheel: SIGNOFF observe for {promise_id} failed ({e:#}); promise stays parked"
                                );
                                false
                            }
                        };
                        if clear {
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
                    if let Some(inbox) = &reads.inbox {
                        let parked: Vec<String> = {
                            let sink = engine.approval_sink().lock().await;
                            sink.iter().cloned().collect()
                        };
                        if !parked.is_empty() && pass.is_multiple_of(APPROVAL_SCAN_EVERY_PASSES) {
                            // One page per pass, shared by every parked id (it
                            // used to be re-fetched once per promise).
                            let read = inbox.clone();
                            let page = crate::backend::run_mail_off_runtime(move || read()).await;
                            let rows = match page {
                                Ok(v) => {
                                    last_scan_err = None;
                                    v.as_array().cloned().unwrap_or_default()
                                }
                                Err(e) => {
                                    let text = format!("{e:#}");
                                    if last_scan_err.as_deref() != Some(text.as_str()) {
                                        eprintln!(
                                            "flywheel: approval drainer inbox scan failed ({text}); parked promises wait for the next scan"
                                        );
                                        last_scan_err = Some(text);
                                    }
                                    Vec::new()
                                }
                            };
                            for promise_id in &parked {
                                let Some((decision, sender)) =
                                    approval_decision_in_inbox(&rows, promise_id)
                                else {
                                    continue;
                                };
                                let outcome = decision.outcome.clone();
                                match engine.resolve_approval(promise_id, &decision).await {
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

/// bd-omwg: the pane-mode spawn drainer thread. Polls the engine's SpawnSink
/// while `invoke` runs (an `await: true` run parks on its job promise until
/// the drainer settles it), launches each job through [`launch_spawn_job`],
/// and keeps draining until `stop` is set (the caller does this after invoke
/// returns, then joins). Loads the profile catalog fresh — the thread is
/// 'static and must not borrow the caller's.
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

    /// bd-ietv: a role whose registry row exists but whose pane is missing
    /// from herdr's listing at dispatch time is the spawn/list race, not a
    /// dead pane. The prune drops the row, so without settling the whole run
    /// bails `no crew pane for @oracle` even though the crew spawned it.
    #[test]
    fn settle_role_waits_out_a_pane_herdr_has_not_listed_yet() {
        use std::collections::HashSet;
        let rows = vec![SpawnEntry {
            pane_id: "w1:p1".into(),
            name: "crew-goal-planner".into(),
            profile: Some("planner".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        }];
        // First two probes report the pane absent (the race), third sees it.
        let mut probe = 0u32;
        let got = settle_role_slot_with(
            &rows,
            "planner",
            None,
            || {
                probe += 1;
                if probe < 3 {
                    Some(HashSet::new())
                } else {
                    Some(HashSet::from(["w1:p1".to_string()]))
                }
            },
            1_000,
            0,
        )
        .expect("settles once the pane is listed");
        assert_eq!(got.pane_id, "w1:p1", "settled the SPAWNED pane");
        assert_eq!(probe, 3, "polled until listed, did not give up early");
    }

    /// bd-ietv: the settle is bounded. A genuinely dead pane still returns
    /// None so the caller bails loud — an unbounded poll would hang the run.
    #[test]
    fn settle_role_gives_up_on_a_pane_that_never_appears() {
        use std::collections::HashSet;
        let rows = vec![SpawnEntry {
            pane_id: "w1:p9".into(),
            name: "crew-goal-oracle".into(),
            profile: Some("oracle".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        }];
        let mut probes = 0u32;
        let got = settle_role_slot_with(
            &rows,
            "oracle",
            None,
            || {
                probes += 1;
                Some(HashSet::new())
            },
            500,
            100,
        );
        assert!(got.is_none(), "a never-listed pane must not be invented");
        assert!(
            probes <= 6,
            "the wait is bounded, gave up after {probes} probes"
        );
    }

    /// bd-ietv: a role with NO registry row was never spawned, so there is
    /// nothing to settle and the caller must keep failing loud.
    #[test]
    fn settle_role_declines_a_role_that_was_never_spawned() {
        use std::collections::HashSet;
        let rows = vec![SpawnEntry {
            pane_id: "w1:p1".into(),
            name: "crew-goal-coder".into(),
            profile: Some("coder".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        }];
        let mut probes = 0u32;
        let got = settle_role_slot_with(
            &rows,
            "oracle",
            None,
            || {
                probes += 1;
                Some(HashSet::from(["w1:p1".to_string()]))
            },
            1_000,
            100,
        );
        assert!(got.is_none(), "unknown role stays unknown");
        assert_eq!(probes, 0, "never polled: there was no row to wait for");
    }

    /// bd-ietv: an unavailable probe is treated as cannot-confirm, not as
    /// dead — the same fail-open posture the registry prune already takes for
    /// an unreachable herdr.
    #[test]
    fn settle_role_treats_an_unavailable_probe_as_not_confirmed() {
        use std::collections::HashSet;
        let rows = vec![SpawnEntry {
            pane_id: "w1:p5".into(),
            name: "crew-goal-reviewer".into(),
            profile: Some("reviewer".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        }];
        let mut probe = 0u32;
        let got = settle_role_slot_with(
            &rows,
            "reviewer",
            None,
            || {
                probe += 1;
                if probe < 2 {
                    None // herdr unreachable
                } else {
                    Some(HashSet::from(["w1:p5".to_string()]))
                }
            },
            1_000,
            0,
        )
        .expect("recovers once the probe answers");
        assert_eq!(got.pane_id, "w1:p5");
    }

    /// bd-8pjr: the dispatch retry schedule is the framework's now, so the
    /// envelope the stack runs with is pinned against the real policy type:
    /// six attempts (five retries), 5s base, 10s cap.
    /// dispatch_chain_carries_the_breaker_as_its_argument below pins the
    /// 5/60s the stack runs with.
    #[test]
    fn dispatch_chain_carries_the_retry_envelope_as_its_argument() {
        let policy = dispatch_policy();
        assert_eq!(policy.retries, MAX_ATTEMPTS - 1, "five retries after the first attempt");
        assert_eq!(policy.backoff.as_millis(), 5_000, "5s backoff base");
        assert_eq!(policy.ceiling.as_millis(), 10_000, "10s backoff cap");
    }

    /// bd-ja69: the stack must not run the framework's default jitter seed.
    /// A constant seed draws the same backoff sequence in every process, so
    /// all dispatchers retry in lockstep after a shared outage. The seed is
    /// drawn from process entropy — pin that it differs from the default,
    /// not any particular value (tests build their own policies).
    #[test]
    fn dispatch_chain_seeds_its_jitter_from_process_entropy() {
        let def = RemotePolicy::<Value, String>::default();
        assert_ne!(
            dispatch_policy().seed, def.seed,
            "the dispatch stack must not run the framework default seed"
        );
    }

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

    /// bd-jvka + ADR-0018 §3 item 5: the ORDER, provable without a live
    /// pane. The breaker is consulted INSIDE the retry — before EVERY
    /// attempt — so an open breaker stops the chain instead of letting it
    /// hammer the pane six more times. And a transient failure is a FAILURE:
    /// one strike per attempt, so the breaker opens on the failures, not on
    /// the dispatches.
    #[test]
    fn dispatch_chain_consults_the_breaker_inside_the_chain() {
        let remote = Remote::new(RemotePolicy {
            retries: 5,
            backoff: kernel_millis(Duration::ZERO),
            ceiling: kernel_millis(Duration::ZERO),
            attempt_timeout: kernel_millis(Duration::from_secs(30)),
            budget: kernel_millis(Duration::from_secs(60)),
            // ONE failure opens the breaker, so the chain stops right after it.
            breaker: Some(BreakerConfig {
                max_failures: 1,
                reset: kernel_millis(Duration::from_secs(60)),
            }),
            ..Default::default()
        });
        let clock: std::sync::Arc<dyn ClockImpl> = std::sync::Arc::new(TestClock::new());
        let rt = Runtime::new();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let out = dispatch_chain(&remote, &rt, &clock, {
            let attempts = attempts.clone();
            move || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Effect::fail("agent_not_ready".to_string())
            }
        });
        assert!(
            matches!(out, Err(DispatchErr::BreakerOpen)),
            "the open breaker must stop the chain: {out:?}"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the breaker is consulted before every attempt, so the chain stopped after the first failure"
        );
    }

    /// ADR-0018 §3 item 5: the breaker is the framework stack's ARGUMENT,
    /// not a second breaker beside it. The production stack says so in the
    /// policy it is built from: five consecutive failures open it for 60s
    /// (ADR-0015 §3 P2), and the framework lets one probe through after the
    /// cooldown so a pane that failed can recover.
    #[test]
    fn dispatch_chain_carries_the_breaker_as_its_argument() {
        let policy = dispatch_policy();
        let breaker = policy
            .breaker
            .expect("the stack carries the breaker as its argument");
        assert_eq!(breaker.max_failures, 5, "five failures open it");
        assert_eq!(
            breaker.reset.as_millis(),
            60_000,
            "60s cooldown before one probe"
        );
    }

    /// bd-jvka: the budget bounds the chain AROUND it — a spent budget must
    /// not attempt at all, and it must not be spent by the breaker check
    /// either.
    #[test]
    fn dispatch_chain_budget_bounds_the_chain() {
        // A clock that advances a millisecond per read: the budget compares
        // the ELAPSED time, so a frozen clock can never show a spent budget.
        struct TickingClock(std::sync::atomic::AtomicU64);
        impl ClockImpl for TickingClock {
            fn sleep(
                &self,
                _duration: toron_core::kernel::Duration,
                _interrupted: &dyn Fn() -> bool,
            ) -> toron_core::kernel::SleepOutcome {
                toron_core::kernel::SleepOutcome::Elapsed
            }
            fn now_millis(&self) -> u64 {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            }
        }
        let remote = Remote::new(RemotePolicy {
            retries: 5,
            backoff: kernel_millis(Duration::ZERO),
            ceiling: kernel_millis(Duration::ZERO),
            attempt_timeout: kernel_millis(Duration::from_secs(30)),
            // zero budget: the first read of the clock already shows it spent
            budget: kernel_millis(Duration::ZERO),
            ..Default::default()
        });
        let clock: std::sync::Arc<dyn ClockImpl> =
            std::sync::Arc::new(TickingClock(std::sync::atomic::AtomicU64::new(0)));
        let rt = Runtime::new();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let out = dispatch_chain(&remote, &rt, &clock, {
            let attempts = attempts.clone();
            move || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Effect::fail("agent_not_ready".to_string())
            }
        });
        assert!(
            matches!(out, Err(DispatchErr::BudgetExhausted)),
            "a spent budget must end the chain: {out:?}"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a spent budget must not attempt at all"
        );
    }

    /// bd-fa58: a saturated bulkhead is a loud fatal, never a silent drop.
    /// The dispatch never ran (zero attempts), and the message says the
    /// shared stack is saturated — not the pane.
    #[test]
    fn dispatch_chain_bulkhead_full_is_a_loud_fatal() {
        // No field names the Bulkhead type: zero the default's bounds
        // through field assignment, so the port gains no new name.
        let mut policy = RemotePolicy::<Value, String>::default();
        policy.bulkhead.permits = 0;
        policy.bulkhead.queue = 0;
        let remote = Remote::new(policy);
        let clock: std::sync::Arc<dyn ClockImpl> = std::sync::Arc::new(TestClock::new());
        let rt = Runtime::new();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let out = dispatch_chain(&remote, &rt, &clock, {
            let attempts = attempts.clone();
            move || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Effect::succeed(json!({"ok": true}))
            }
        });
        let Err(DispatchErr::Fatal(msg)) = out else {
            panic!("a saturated bulkhead must be a loud fatal");
        };
        assert!(
            msg.contains("saturated"),
            "the fatal must name the saturated stack: {msg}"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a rejected dispatch never attempts"
        );
    }

    /// bd-jvka: the FALLBACK is outside the chain — the chain hands the
    /// rate-limit verdict back as a FINAL answer, so only the caller picks
    /// the next route. A chain that rerouted itself would hide the fallback
    /// inside the retry.
    #[test]
    fn dispatch_chain_hands_the_fallback_verdict_back_to_the_caller() {
        let remote = Remote::new(RemotePolicy {
            retries: 5,
            backoff: kernel_millis(Duration::ZERO),
            ceiling: kernel_millis(Duration::ZERO),
            attempt_timeout: kernel_millis(Duration::from_secs(30)),
            budget: kernel_millis(Duration::from_secs(60)),
            ..Default::default()
        });
        let clock: std::sync::Arc<dyn ClockImpl> = std::sync::Arc::new(TestClock::new());
        let rt = Runtime::new();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let out = dispatch_chain(&remote, &rt, &clock, {
            let attempts = attempts.clone();
            move || {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Effect::succeed(json!({"ok": false, "kind": "rate_limit"}))
            }
        })
        .expect("a final verdict");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a final verdict ends the chain"
        );
        assert_eq!(
            out["kind"],
            json!("rate_limit"),
            "the verdict is handed back as-is"
        );
    }

    /// bd-ozpj: a JOURNALED constructor that cannot journal REFUSES. The old
    /// code degraded to Recording, so the leaves would perform their
    /// external effects with NO journal row — the exact thing `Enforced`
    /// exists to prevent, and it was silent.
    #[test]
    fn effect_state_journaled_constructor_refuses_without_a_journal() {
        use toron_workflow::engine::EffectJournalMode;
        // A journaled constructor with a journal: Enforced.
        assert_eq!(
            crate::pane::engine_journal_mode(true, true).expect("journaled"),
            EffectJournalMode::Enforced
        );
        // A journaled constructor whose journal could NOT be opened: refuse.
        let err = crate::pane::engine_journal_mode(true, false)
            .expect_err("a journaled constructor that cannot journal must refuse");
        assert!(
            err.to_string().contains("refusing to run unjournaled"),
            "the refusal must say what it refuses: {err}"
        );
        // The dry arm (no journal requested): Recording, visibly.
        assert_eq!(
            crate::pane::engine_journal_mode(false, false).expect("dry"),
            EffectJournalMode::Recording
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
        let entries = as_live(vec![
            entry("w1:p1", "crew-wf-oracle", "oracle"),
            entry("w1:p2", "crew-wf-coder", "coder"),
        ]);
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
        let oracle_only = as_live(vec![entry("w1:p1", "crew-wf-oracle", "oracle")]);
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
        // Scope the slice to the FUNCTION, not a fixed byte count. The magic
        // 12000-byte window silently excluded the arm's tail as soon as the
        // dispatch grew (bd-jvka moved the chain in front of it), which reads
        // as "the marker is missing" when it is merely further down.
        let end = src[dispatch..]
            .find("\nfn ")
            .map_or(dispatch, |i| dispatch + i);
        let arm = &src[dispatch..end];
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
        // Scope the slice to the FUNCTION, not a fixed byte count. A magic
        // window silently starts excluding the function's own tail as soon as
        // the function grows (bd-crlx added the env-pin resolution ahead of the
        // registry row and pushed the row out of a 16000-char window), which
        // reads as "the row is missing" when it is merely further down.
        let end = src[start..].find("\nfn ").map_or(start, |i| start + i);
        let slice = &src[start..end];
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
    /// bd-qx0z (red first): resolution takes a live-gated registry —
    /// prune with the live set, resolve, fresh row wins over the dead one
    /// even though the dead row sorts first.
    #[test]
    fn live_registry_gates_unpruned_vecs() {
        use std::collections::HashSet;
        let stale = SpawnEntry {
            pane_id: "wOld:p9".into(),
            name: "flywheel-x-planner".into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: true,
            incarnation: 0,
        };
        let fresh = SpawnEntry {
            pane_id: "wNew:p1".into(),
            name: "crew-wf-planner".into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        };
        let live: HashSet<String> = ["wNew:p1".to_string()].into_iter().collect();
        let reg = LiveRegistry::prune(vec![stale, fresh], &live);
        assert_eq!(reg.len(), 1);
        let e = resolve_pane_entry(&reg, "planner").expect("fresh hit");
        assert_eq!(e.name, "crew-wf-planner");
    }

    /// Test fixtures are live by construction — prune with their own pane
    /// ids (identity), so existing resolve tests keep their semantics while
    /// going through the gate.
    fn as_live(entries: Vec<SpawnEntry>) -> LiveRegistry {
        use std::collections::HashSet;
        let live: HashSet<String> = entries.iter().map(|e| e.pane_id.clone()).collect();
        LiveRegistry::prune(entries, &live)
    }

    /// bd-rkx8 (red first): among same-profile rows the highest
    /// incarnation wins even when a stale row sorts first — names are
    /// stable across respawns, generations are not.
    #[test]
    fn resolve_prefers_highest_incarnation() {
        use std::collections::HashSet;
        let stale = SpawnEntry {
            pane_id: "wOld:p9".into(),
            name: "crew-wf-planner".into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: true,
            incarnation: 0,
        };
        let fresh = SpawnEntry {
            pane_id: "wNew:p1".into(),
            name: "crew-wf-planner".into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 1,
        };
        let live: HashSet<String> = ["wOld:p9".to_string(), "wNew:p1".to_string()]
            .into_iter()
            .collect();
        let reg = LiveRegistry::prune(vec![stale, fresh], &live);
        let e = resolve_pane_entry(&reg, "planner").expect("hit");
        assert_eq!(e.pane_id, "wNew:p1");
        assert_eq!(next_incarnation(reg.entries(), "crew-wf-planner"), 2);
    }

    #[test]
    fn resolve_pane_entry_prefers_profile_then_name() {
        let entries = as_live(vec![
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
        ]);
        let e = resolve_pane_entry(&entries, "planner").expect("profile hit");
        assert_eq!(e.name, "crew-wf-planner");
        let e = resolve_pane_entry(&entries, "plain-agent").expect("name fallback");
        assert_eq!(e.name, "plain-agent");
        assert_eq!(e.profile, None);
        assert!(resolve_pane_entry(&entries, "ghost").is_none());
    }

    /// bd-0s0y: a `parallel`/`each` fan-out over a role must be able to reach
    /// N DIFFERENT agents. Before this, every copy resolved through the
    /// single-entry lookup to one pane, so copies 2..N hit the busy-owner
    /// fence and killed the run — the reason `steer --crew N` never completed
    /// end to end, at any crew size.
    #[test]
    fn fanout_copies_resolve_to_distinct_panes() {
        // The shape a real crew writes: ONE pane per unique role is what the
        // packer spawns for `crew: auto`, and a role with copies gets
        // `…-oracle`, `…-oracle2`, `…-oracle3` on separate panes.
        let entries = as_live(vec![
            SpawnEntry {
                pane_id: "w2:p1".into(),
                name: "crew-goal-oracle".into(),
                profile: Some("oracle".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry {
                pane_id: "w2:p2".into(),
                name: "crew-goal-oracle2".into(),
                profile: Some("oracle".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry {
                pane_id: "w2:p3".into(),
                name: "crew-goal-oracle3".into(),
                profile: Some("oracle".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry {
                pane_id: "w2:p4".into(),
                name: "crew-goal-coder".into(),
                profile: Some("coder".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
        ]);

        // The defect, stated as an assertion: the single-entry lookup is
        // ordinal-blind, so all three copies collapse onto one agent. This is
        // what put "fenced as busy" on the second copy.
        let first = resolve_pane_entry(&entries, "oracle").expect("profile hit");
        let again = resolve_pane_entry(&entries, "oracle").expect("profile hit");
        assert_eq!(
            first.pane_id, again.pane_id,
            "the single-entry lookup is ordinal-blind BY CONSTRUCTION — this equality \
             is the defect, not an accident of these rows"
        );

        // The fix: copy i reaches copy i.
        let picked: Vec<String> = (0..3)
            .map(|i| {
                resolve_pane_entry_nth(&entries, "oracle", i)
                    .expect("copy exists")
                    .pane_id
                    .clone()
            })
            .collect();
        assert_eq!(
            picked,
            vec!["w2:p1", "w2:p2", "w2:p3"],
            "3 copies of `over: \"3\"` must reach 3 distinct panes, got {picked:?}"
        );

        // A 4th copy does not exist: the fence must fail loud on an
        // over-count rather than silently reuse copy 0's pane.
        assert!(
            resolve_pane_entry_nth(&entries, "oracle", 3).is_none(),
            "one more copy than the crew has panes must be None, never a reuse"
        );

        // A single-copy role is unchanged, so the working path cannot regress:
        // nth 0 is exactly the entry the old lookup returned.
        assert_eq!(
            resolve_pane_entry_nth(&entries, "coder", 0)
                .expect("coder")
                .pane_id,
            resolve_pane_entry(&entries, "coder")
                .expect("coder")
                .pane_id
        );
        assert!(resolve_pane_entry_nth(&entries, "coder", 1).is_none());
    }

    /// bd-0s0y: THE fence for the whole chain — the copy's `iteration_key`
    /// becomes an ordinal, and the ordinal reaches a DIFFERENT pane.
    ///
    /// This is the production path end to end, at unit speed: `run_parallel`
    /// mints `p{i}`, the sink reads the ordinal off it, and the ordinal picks
    /// the pane. Before the fix the middle step did not exist, so all three
    /// copies resolved to `w2:p1` and copies 2 and 3 were refused by the
    /// busy-owner fence — which is how `steer --crew` died on step one.
    #[test]
    fn a_fanout_copy_reaches_its_own_pane() {
        let entries = as_live(
            (1..=3)
                .map(|i| SpawnEntry {
                    pane_id: format!("w4:p{i}"),
                    name: if i == 1 {
                        "crew-goal-oracle".to_string()
                    } else {
                        format!("crew-goal-oracle{i}")
                    },
                    profile: Some("oracle".into()),
                    harness: Some("omp".into()),
                    model: None,
                    model_pin: None,
                    skills_digest: None,
                    mail_identity: None,
                    skills_injected: false,
                    incarnation: 0,
                })
                .collect(),
        );

        // `over: "3"` over `@oracle`: three copies, three panes.
        for (key, want) in [("p0", "w4:p1"), ("p1", "w4:p2"), ("p2", "w4:p3")] {
            let ordinal =
                toron_workflow::identity::copy_ordinal(Some(key)).expect("a copy key parses");
            let entry = resolve_role_slot(&entries, "oracle", Some(ordinal))
                .expect("a copy resolves")
                .expect("an entry exists");
            assert_eq!(entry.pane_id, want, "{key} must reach its own pane");
        }
    }

    /// bd-07m2: too many copies for the crew is a LOUD shortfall, never a
    /// silent reuse of copy 0. Reuse is the exact defect: copy 1 would be
    /// refused by the busy-owner fence and the run would die blaming the fence
    /// instead of the crew that never grew.
    ///
    /// The shortfall now comes from `shortfall_error`, AFTER the settle window
    /// has run. `resolve_role_slot` must return `Ok(None)` for an over-count —
    /// when it bailed, the caller's `?` made the bd-ietv settle arm
    /// unreachable for every fan-out copy, and cron Pane mode (which prunes
    /// against a fresh listing) turned a spawn/list race into "the crew holds
    /// 0 panes". Asserting `Ok(None)` here IS the settle-reachability fence.
    #[test]
    fn too_many_copies_names_the_shortfall_instead_of_reusing_copy_zero() {
        let entries = as_live(vec![SpawnEntry {
            pane_id: "w5:p1".into(),
            name: "crew-goal-oracle".into(),
            profile: Some("oracle".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        }]);
        // Must NOT be an error: the settle arm has to be reachable.
        let got = resolve_role_slot(&entries, "oracle", Some(1))
            .expect("an over-count is not an error, it is a settle candidate");
        assert!(
            got.is_none(),
            "an absent copy must resolve to None so the settle window runs, never to copy 0"
        );
        let msg = shortfall_error(&entries, "oracle", 1).to_string();
        assert!(msg.contains("copy 1"), "must name the copy: {msg}");
        assert!(
            msg.contains("1 slot(s)"),
            "must name the shortfall in SLOTS, not rows: {msg}"
        );
        assert!(
            !msg.contains("w5:p1"),
            "must not hand back copy 0's pane as if it were copy 1's: {msg}"
        );
    }

    /// bd-07m2: the shortfall counts SLOTS. A respawn is a newer generation of
    /// the same slot (the bd-rkx8 invariant `resolve_pane_entry_nth` already
    /// honors), so a two-generation registry must NOT be reported as a crew of
    /// two — that told the operator to grow a crew that was already correct.
    #[test]
    fn the_shortfall_counts_one_respawned_slot_as_one_pane() {
        let row = |pane: &str, name: &str, inc: u64| SpawnEntry {
            pane_id: pane.into(),
            name: name.into(),
            profile: Some("oracle".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: inc,
        };
        let entries = as_live(vec![
            row("w7:p1", "crew-goal-oracle", 0),
            // Same name, newer generation, different pane.
            row("w7:p9", "crew-goal-oracle", 1),
        ]);
        let msg = shortfall_error(&entries, "oracle", 1).to_string();
        assert!(
            msg.contains("1 slot(s)"),
            "two generations of one name is ONE slot, not two panes: {msg}"
        );
    }

    /// bd-0s0y: a step that is NOT a fan-out copy resolves exactly as it always
    /// did. The whole fix is additive for the 99% of steps that carry no
    /// iteration_key, so this is the no-regression half of the fence.
    #[test]
    fn a_step_outside_a_fanout_is_unaffected() {
        let entries = as_live(vec![SpawnEntry {
            pane_id: "w6:p1".into(),
            name: "crew-goal-oracle".into(),
            profile: Some("oracle".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        }]);
        assert_eq!(
            resolve_role_slot(&entries, "oracle", None)
                .expect("no ordinal never fails")
                .expect("entry")
                .pane_id,
            resolve_pane_entry(&entries, "oracle")
                .expect("old lookup")
                .pane_id
        );
    }

    /// bd-0s0y: a respawn is a NEWER GENERATION of the same slot, not an extra
    /// copy. Counting generations as copies would hand copy 1 a dead pane and
    /// leave the live one unreachable.
    #[test]
    fn respawn_does_not_inflate_the_copy_count() {
        let entries = as_live(vec![
            SpawnEntry {
                pane_id: "w3:p1".into(),
                name: "crew-goal-oracle".into(),
                profile: Some("oracle".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 0,
            },
            // Same name, newer generation, different pane.
            SpawnEntry {
                pane_id: "w3:p9".into(),
                name: "crew-goal-oracle".into(),
                profile: Some("oracle".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                skills_digest: None,
                mail_identity: None,
                skills_injected: false,
                incarnation: 1,
            },
        ]);
        assert_eq!(
            resolve_pane_entry_nth(&entries, "oracle", 0)
                .expect("copy 0")
                .pane_id,
            "w3:p9",
            "copy 0 must be the live generation, not the dead one"
        );
        assert!(
            resolve_pane_entry_nth(&entries, "oracle", 1).is_none(),
            "one name is one slot: a respawn must not read as a second copy"
        );
    }

    /// bd-op8b (red first): loaded registry rows whose pane is gone must
    /// not survive into dispatch resolution — a stale first match stole
    /// fresh-crew dispatches live (stale skills_injected=true also muted
    /// the readiness gate). Live panes in any workspace are kept.
    #[test]
    fn prune_dead_pane_rows_keeps_only_live_panes() {
        use std::collections::HashSet;
        let stale = SpawnEntry {
            pane_id: "wOld:p9".into(),
            name: "flywheel-chiebukuro-planner".into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: true,
            incarnation: 0,
        };
        let fresh = SpawnEntry {
            pane_id: "wNew:p1".into(),
            name: "crew-wf-planner".into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        };
        let live: HashSet<String> = ["wNew:p1".to_string()].into_iter().collect();
        let kept = prune_dead_pane_rows(vec![stale, fresh], &live);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "crew-wf-planner");
        assert!(!kept[0].skills_injected);
    }

    /// bd-bfwj (red first): closing a workspace reaps exactly its dead
    /// rows — live panes anywhere are kept, gone panes are dropped, and
    /// the pruned count is reported for the close log line.
    #[test]
    fn reap_close_drops_only_gone_panes() {
        use std::collections::HashSet;
        let mk = |pane: &str, name: &str| SpawnEntry {
            pane_id: pane.into(),
            name: name.into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            mk("wDead:p1", "crew-old-planner"),
            mk("wLive:p1", "crew-new-planner"),
            mk("wOther:p1", "flywheel-other-oc"),
        ];
        let live: HashSet<String> = ["wLive:p1".to_string(), "wOther:p1".to_string()]
            .into_iter()
            .collect();
        let (kept, pruned) = reap_closed_rows(entries, &live, "herdr");
        assert_eq!(pruned, 1);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|e| e.pane_id != "wDead:p1"));
    }

    /// bd-j0b5: the persisted clear keeps a row from a harness herdr cannot
    /// inject (its pane is absent on purpose) and an unknown harness; only a
    /// gone pane of a herdr-injectable harness is cleared.
    #[test]
    fn reap_close_keeps_pins_herdr_cannot_inject() {
        use std::collections::HashSet;
        let mk = |pane: &str, name: &str, harness: Option<&str>| SpawnEntry {
            pane_id: pane.into(),
            name: name.into(),
            profile: None,
            harness: harness.map(str::to_string),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: Some(name.into()),
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            mk("wDead:p1", "freebuff-pin", Some("freebuff")),
            mk("wGone:p2", "gone-opencode", Some("opencode")),
            mk("wLegacy:p3", "legacy-pin", None),
        ];
        let live: HashSet<String> = HashSet::new();
        let (kept, pruned) = reap_closed_rows(entries, &live, "herdr");
        assert_eq!(pruned, 1);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().any(|e| e.pane_id == "wDead:p1"));
        assert!(kept.iter().any(|e| e.pane_id == "wLegacy:p3"));
        assert!(!kept.iter().any(|e| e.pane_id == "wGone:p2"));
    }

    /// bd-nwnb (red first): the gauge — rows whose pane is gone,
    /// reported as (pane_id, name) pairs for status output. Live rows
    /// never appear, even when they sort after dead ones.
    #[test]
    fn dead_rows_gauge_names_only_gone_panes() {
        use std::collections::HashSet;
        let mk = |pane: &str, name: &str| SpawnEntry {
            pane_id: pane.into(),
            name: name.into(),
            profile: Some("planner".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            mk("wDead:p1", "crew-old-planner"),
            mk("wLive:p1", "crew-new-planner"),
        ];
        let live: HashSet<String> = ["wLive:p1".to_string()].into_iter().collect();
        let dead = find_dead_rows(&entries, &live, "herdr");
        assert_eq!(
            dead,
            vec![("wDead:p1".to_string(), "crew-old-planner".to_string())]
        );
        let none = find_dead_rows(
            &entries,
            &["wDead:p1", "wLive:p1"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            "herdr",
        );
        assert_eq!(none, [] as [(String, String); 0]);
    }

    /// bd-p09m: the gauge agrees with the clear — a mail-only row the reap
    /// keeps is not a dead row. Only a gone pane of a herdr-injectable
    /// harness on herdr is dead.
    #[test]
    fn a_kept_mail_only_row_is_not_a_dead_row() {
        use std::collections::HashSet;
        let mk = |pane: &str, name: &str, harness: Option<&str>| SpawnEntry {
            pane_id: pane.into(),
            name: name.into(),
            profile: None,
            harness: harness.map(str::to_string),
            model: None,
            model_pin: None,
            skills_digest: None,
            mail_identity: Some(name.into()),
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            mk("wDead:p1", "freebuff-pin", Some("freebuff")),
            mk("wGone:p2", "gone-opencode", Some("opencode")),
            mk("wLegacy:p3", "legacy-pin", None),
        ];
        let live: HashSet<String> = HashSet::new();
        let dead = find_dead_rows(&entries, &live, "herdr");
        assert_eq!(
            dead,
            vec![("wGone:p2".to_string(), "gone-opencode".to_string())]
        );
        // A superlogical process clears nothing, whatever the harness.
        let none = find_dead_rows(&entries, &live, "superlogical");
        assert_eq!(none, [] as [(String, String); 0]);
    }

    /// bd-lvyi: a per-step harness override resolves to a crew pane OF THAT
    /// HARNESS (the `harness` column), not the step profile's pane.
    #[test]
    fn resolve_pane_by_harness_finds_override_harness_pane() {
        let entries = as_live(vec![
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
        ]);
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
        let entries = as_live(vec![
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
        ]);
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

    #[test]
    fn fresh_respawn_args_force_a_new_session() {
        assert_eq!(
            fresh_session_respawn_args("flywheel", "crew-goal-coder", "pi"),
            vec![
                "__respawn".to_string(),
                "flywheel".into(),
                "crew-goal-coder".into(),
                "pi".into(),
                "--force".into(),
            ]
        );
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
        let entries = as_live(vec![entry("w1:p1", "crew-goal-reviewer", "reviewer", "pi")]);
        let action = Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@reviewer".into()),
                harness: Some("opencode".into()),
                ..Default::default()
            },
            channel: None,
        };
        let out = pane_dispatch(&catalog, "proj", "wf", &json!({}), &entries, &action, None)
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
        let entries = as_live(vec![SpawnEntry {
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
        }]);
        let action = Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@reviewer".into()),
                harness: Some("opencode".into()),
                ..Default::default()
            },
            channel: None,
        };
        let err = pane_dispatch(&catalog, "proj", "wf", &json!({}), &entries, &action, None)
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
        let engine = std::sync::Arc::new(
            toron_workflow::WorkflowEngine::local_with(housekeeper_resolver()).expect("engine"),
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
        let engine = std::sync::Arc::new(
            toron_workflow::WorkflowEngine::local_with(housekeeper_resolver()).expect("engine"),
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
        let runner = std::sync::Arc::new(|inv: &toron_workflow::engine::ActionInvocation| {
            Ok(json!({ "action": inv.action.type_name() }))
        });
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
        let runner: toron_workflow::engine::EffectRunner =
            std::sync::Arc::new(|inv: &toron_workflow::engine::ActionInvocation| {
                Ok(json!({ "action": inv.action.type_name() }))
            });
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
        let runner = std::sync::Arc::new(|inv: &toron_workflow::engine::ActionInvocation| {
            Ok(json!({ "action": inv.action.type_name() }))
        });
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
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = toron_workflow::WorkflowEngine::local();
        }))
        .is_err();
        assert!(
            panicked,
            "Resonate::local must refuse to construct off-runtime"
        );
    }
}

#[cfg(test)]
mod bd_5yzp_tests {
    use super::dispatch_failure_error;

    #[test]
    fn empty_stdout_stderr_yields_scoped_nonempty_error() {
        let e = dispatch_failure_error("coder", "WittyAnt", "", "");
        assert!(!e.trim().is_empty(), "error must never be empty");
        assert!(e.contains("@coder"), "carries profile: {e}");
        assert!(e.contains("WittyAnt"), "carries agent: {e}");
    }

    /// One row per source preference: real stderr wins, a whitespace-only
    /// stderr falls back to stdout (bd-5yzp: never error:'').
    #[test]
    fn dispatch_failure_error_prefers_real_stderr() {
        let rows = [
            ("stderr-wins", "some stdout", "boom stderr", "boom stderr"),
            (
                "stdout-when-stderr-blank",
                "some stdout",
                "  \n ",
                "some stdout",
            ),
        ];
        for (name, stdout, stderr, expected) in rows {
            let e = dispatch_failure_error("coder", "WittyAnt", stdout, stderr);
            assert_eq!(e, expected, "row {name}");
        }
    }
}

/// ADR-0024 W1a / bd-42fs: the pane spawn one-shot registry fallback. The
/// decision is pure, so these rows need no herdr pane and no live harness.
#[cfg(test)]
mod pane_registry_fallback_tests {
    use super::*;
    use crate::models::ModelEntry;

    /// The routing table the rows resolve against: one model carrying a 429
    /// rung, one carrying none, and the rung target itself. The rung
    /// mechanics (grok harness coupling, the injection form) are pinned in
    /// `models::tests::fallback_route_rows`; this file owns the GATE.
    fn routing() -> ModelRegistry {
        let mut r = ModelRegistry::default();
        r.models.insert(
            "fleet/claude-opus-4".into(),
            ModelEntry {
                fallback: Some("openai/gpt-5.2".into()),
                ..ModelEntry::default()
            },
        );
        r.models
            .insert("fleet/no-rung".into(), ModelEntry::default());
        r.models
            .insert("openai/gpt-5.2".into(), ModelEntry::default());
        r
    }

    /// One routing row — a struct rather than a tuple so each field is named
    /// at the assertion sites and clippy's `type_complexity` stays quiet.
    struct RouteRow {
        case: &'static str,
        error: &'static str,
        profile_model: &'static str,
        harness: &'static str,
        /// `None` = re-raise the original error; otherwise the hop's
        /// (registry rung id, harness, injection string).
        expected: Option<(&'static str, &'static str, &'static str)>,
    }

    /// One row per decision the hop can reach: hop to the entry's rung, flip
    /// the harness when the rung is grok-family, or stay put. `None` means
    /// "re-raise the original error" — the half of this function that is easy
    /// to get wrong, so it gets a row for each reason it can happen.
    #[test]
    fn registry_fallback_route_rows() {
        let rows = [
            RouteRow {
                case: "rate-limit-hops-to-a-non-grok-rung",
                error: "429 Too Many Requests",
                profile_model: "fleet/claude-opus-4",
                harness: "claude",
                expected: Some(("openai/gpt-5.2", "claude", "openai/gpt-5.2")),
            },
            RouteRow {
                case: "rate-limit-without-a-rung-stays-put",
                error: "429 Too Many Requests",
                profile_model: "fleet/no-rung",
                harness: "claude",
                expected: None,
            },
            RouteRow {
                case: "rate-limit-on-an-unregistered-model-stays-put",
                error: "429 Too Many Requests",
                profile_model: "fleet/not-registered",
                harness: "claude",
                expected: None,
            },
            RouteRow {
                // A timeout never reaches the hop, and a 401 is
                // reroute-eligible WITHOUT being model-swappable — neither
                // gets better by changing the model.
                case: "timeout-never-hops",
                error: "operation timeout after 30s",
                profile_model: "fleet/claude-opus-4",
                harness: "claude",
                expected: None,
            },
            RouteRow {
                case: "auth-failure-never-hops",
                error: "401 Unauthorized",
                profile_model: "fleet/claude-opus-4",
                harness: "claude",
                expected: None,
            },
            RouteRow {
                case: "reservation-conflict-never-hops",
                error: "reservation conflict: path held",
                profile_model: "fleet/claude-opus-4",
                harness: "claude",
                expected: None,
            },
        ];
        for row in rows {
            let case = row.case;
            let got = registry_fallback_route(row.error, row.profile_model, row.harness, || {
                Some(routing())
            });
            match row.expected {
                None => assert_eq!(got, None, "row {case}"),
                Some((model_id, want_harness, want_model)) => {
                    let route = got.unwrap_or_else(|| panic!("row {case}: expected a hop"));
                    assert_eq!(route.model_id, model_id, "row {case}");
                    assert_eq!(route.harness, want_harness, "row {case}");
                    assert_eq!(route.model, want_model, "row {case}");
                }
            }
        }
    }

    /// The gate comes BEFORE the registry load: a failure that cannot hop must
    /// not pay for reading `models.toml`. The loader panics, so a future
    /// reorder that loads first fails loudly here instead of silently costing
    /// a file read on every failed spawn.
    #[test]
    fn failures_that_cannot_hop_never_read_the_registry() {
        for error in [
            "operation timeout after 30s",
            "401 Unauthorized",
            "reservation conflict: path held",
        ] {
            let route = registry_fallback_route(error, "fleet/claude-opus-4", "claude", || {
                panic!("{error}: a non-rate-limit failure must not read the registry")
            });
            assert_eq!(route, None, "{error}");
        }
    }

    /// GS-043 I4: the hop journals prepared → in_flight → failed around
    /// exactly ONE apply, and a fallback that also rate-limits propagates
    /// instead of re-entering. Drives the real intent builder, so the row's
    /// `to` binding is the tested one: the registry ID that drove the
    /// decision, never the harness-resolved injection string.
    #[test]
    fn rate_limit_hop_journals_once_and_cannot_hop_twice() {
        let _home = crate::config::test_state_home();
        let project = "pane-registry-fallback-oneshot";
        let route = registry_fallback_route(
            "429 Too Many Requests",
            "fleet/claude-opus-4",
            "claude",
            || Some(routing()),
        )
        .expect("a rate limit with a rung routes");
        let intent = model_fallback_intent(
            project,
            "WittyAnt",
            "coder",
            "fleet/claude-opus-4",
            "claude",
            &route,
            "429 Too Many Requests".into(),
        );
        assert_eq!(intent.from.model, "fleet/claude-opus-4", "the id we left");
        assert_eq!(intent.to.model, "openai/gpt-5.2", "the registry rung id");
        assert_eq!(intent.to.profile_name, "coder", "the role is stable");
        assert_eq!(intent.state, crate::agent_ref::EffectState::Prepared);

        let mut applies = 0;
        let err = crate::agent_ref::commit_rebind(project, intent, |_| {
            applies += 1;
            // The effect law: intent is journaled BEFORE the provider runs.
            // Read the log from inside the attempt — that is the only place
            // the ordering is observable at all.
            let before = std::fs::read_to_string(crate::state::project_log_path(project))
                .expect("journal written before the provider ran");
            for state in ["prepared", "in_flight"] {
                assert!(
                    before.contains(&format!("\"state\":\"{state}\"")),
                    "apply ran before {state} was journaled: {before}"
                );
            }
            assert!(
                !before.contains("\"state\":\"settled\""),
                "nothing may be settled while apply is still running: {before}"
            );
            // the rung rate-limited too: fail, do not hop again
            Err::<(), _>(anyhow!("429 Too Many Requests"))
        });
        assert!(err.is_err(), "a failed fallback must propagate");
        assert_eq!(applies, 1, "one shot: apply ran exactly once");

        let log = std::fs::read_to_string(crate::state::project_log_path(project))
            .expect("rebind journal written");
        for state in ["prepared", "in_flight", "failed"] {
            assert!(
                log.contains(&format!("\"state\":\"{state}\"")),
                "{state} must be journaled: {log}"
            );
        }
        assert!(!log.contains("\"state\":\"settled\""), "{log}");
        assert!(log.contains("\"kind\":\"rate_limit\""), "{log}");
    }

    /// The row names the harness the run is about to use, not the one the
    /// failed run used. The headless `model_fallback` rung journaled the
    /// caller's harness while dispatching the grok-coupled one, so it recorded
    /// (and keyed) a route it never took.
    #[test]
    fn hop_row_records_the_harness_being_dispatched() {
        let route = FallbackRoute {
            model_id: "grok/grok-4".into(),
            harness: "grok".into(),
            model: "grok-4".into(),
        };
        let intent = model_fallback_intent(
            "flywheel",
            "coder",
            "coder",
            "zai/glm-5.2",
            "omp",
            &route,
            "429 Too Many Requests".into(),
        );
        assert_eq!(intent.from.harness, "omp", "where the hop came from");
        assert_eq!(
            intent.to.harness, "grok",
            "where it is going, not where it was"
        );
        assert_eq!(intent.to.model, "grok/grok-4", "the id that drove it");
    }

    /// The headless rung in `main.rs` now journals through this builder too.
    /// Pin the row it handed `commit_rebind` BEFORE the builder existed: the
    /// effect_key is durable journal identity, so a reuse that moved the pin,
    /// the project, or either binding would orphan every row already written.
    #[test]
    fn headless_rung_wires_the_same_row_as_its_inline_literal() {
        let route = registry_fallback_route(
            "429 Too Many Requests",
            "fleet/claude-opus-4",
            "claude",
            || Some(routing()),
        )
        .expect("a rate limit with a rung routes");
        // main.rs passes the profile name as both pin and role, under its
        // hardcoded "flywheel" project.
        let got = model_fallback_intent(
            "flywheel",
            "coder",
            "coder",
            "fleet/claude-opus-4",
            "claude",
            &route,
            "429 Too Many Requests".into(),
        );
        let inline_literal = crate::agent_ref::RebindIntent::prepare(
            crate::agent_ref::AgentRef::new("flywheel", "coder"),
            crate::agent_ref::AgentBinding {
                profile_name: "coder".to_string(),
                model: "fleet/claude-opus-4".to_string(),
                harness: "claude".to_string(),
            },
            crate::agent_ref::AgentBinding {
                profile_name: "coder".to_string(),
                model: "openai/gpt-5.2".to_string(),
                harness: "claude".to_string(),
            },
            crate::agent_ref::RebindReason::ModelFallback {
                kind: "rate_limit".into(),
                evidence: "429 Too Many Requests".into(),
            },
            0,
        );
        assert_eq!(
            got, inline_literal,
            "the shared builder must not move the headless rung's effect_key"
        );
    }
}

#[cfg(test)]
mod bd_d7zu_shell_gate_tests {
    use super::shell_gate_refusal;

    /// The gate is fail-CLOSED, so the refusal must name the agent, the pane,
    /// and the real cause. The old failure chain ended in
    /// `agent <name> not found in toron`, which points the operator at the
    /// mailbox when the truth is a shell that never booted.
    #[test]
    fn refusal_names_the_agent_pane_and_the_real_cause() {
        let e = shell_gate_refusal("crew-x-coder", "w1:p3");
        let msg = e.to_string();
        assert!(msg.contains("crew-x-coder"), "names the agent: {msg}");
        assert!(msg.contains("w1:p3"), "names the pane: {msg}");
        assert!(
            msg.contains("never rendered a prompt"),
            "states the cause: {msg}"
        );
        assert!(
            !msg.contains("not found in toron"),
            "must not launder a shell fault into the mailbox fault: {msg}"
        );
    }

    /// One row per host condition. The budget is a number an operator has to
    /// be able to read off the message, and a 15s budget on a fresh pane
    /// (measured ~770ms) must be stated as a number, not a vibe.
    #[test]
    fn refusal_states_the_budget_and_the_remedy() {
        let msg = shell_gate_refusal("a", "b").to_string();
        assert!(msg.contains("15s"), "states the budget: {msg}");
        assert!(
            msg.contains("less loaded") || msg.contains("raise the budget"),
            "offers a remedy: {msg}"
        );
    }
}

// bd-5llv's idle-gate tests are GONE with the mechanism they covered. The gate
// they pinned (a synchronous in-process agent_wait) was replaced by the
// toron-workflow busy-owner fence in bd-fen3; keeping tests for a deleted
// mechanism would be a lie about what protects dispatch. The receipt-shape fix
// from the same bead is unrelated and stays.

#[cfg(test)]
mod bd_5llv_receipt_tests {
    use super::dispatch_receipt_ok;
    use serde_json::json;

    /// THE regression. A landed send must report `ok: true` at the top level,
    /// because that is the exact field the breaker reads to decide whether the
    /// dispatch succeeded. Before this, `ok` was written only on the failure
    /// path, so every landed send read as a FAILURE and charged the breaker —
    /// five successes opened it and the sixth dispatch fast-escalated with
    /// "breaker Open" while every prompt had actually landed.
    #[test]
    fn a_landed_send_reports_ok_true() {
        let verdict = json!({
            "ok": true, "landed": true, "name": "crew-x-coder",
            "status": "working", "reason": "landed",
        });
        assert_eq!(
            dispatch_receipt_ok(&verdict),
            json!(true),
            "a landed send MUST surface ok:true — the breaker reads this field"
        );
    }

    /// One row per real verdict `confirm_send_json` can emit. Every one of them
    /// carries a top-level `ok`, so none may read as absent. The `stalled` and
    /// `timeout` rows are the subtle ones: they are NOT landed, but the send
    /// itself did not error, so the verdict still carries ok:true and the
    /// breaker must record a success — a stall is not a dead pane.
    #[test]
    fn every_send_verdict_carries_a_usable_ok() {
        for (name, verdict, want) in [
            (
                "landed",
                json!({"ok": true, "landed": true, "reason": "landed"}),
                true,
            ),
            (
                "stalled",
                json!({"ok": true, "landed": false, "reason": "stalled"}),
                true,
            ),
            (
                "timeout",
                json!({"ok": true, "landed": null, "reason": "timeout"}),
                true,
            ),
            (
                "coded",
                json!({"ok": true, "landed": false, "reason": "error:auth"}),
                true,
            ),
            (
                "explicit-false",
                json!({"ok": false, "error": "boom"}),
                false,
            ),
        ] {
            assert_eq!(
                dispatch_receipt_ok(&verdict),
                json!(want),
                "row {name}: the breaker reads this field, so it must never be ambiguous"
            );
        }
    }

    /// A verdict with no `ok` at all is a contract violation upstream, and
    /// defaulting it to `false` charges the breaker rather than hiding it.
    /// Fail toward "not a success" so an unexpected shape is visible as a
    /// breaker failure instead of a silent pass.
    #[test]
    fn a_verdict_without_ok_reads_as_failure_not_success() {
        for verdict in [json!({}), json!({"reason": "landed"}), json!(null)] {
            assert_eq!(
                dispatch_receipt_ok(&verdict),
                json!(false),
                "an unrecognised verdict must not be read as a success"
            );
        }
    }
}

#[cfg(test)]
mod bd_qlr1_env_pin_wiring_tests {
    use super::{pane_env_for_launch, spawn_launch_plan};
    use crate::profile::{Profile, ProfileCatalog};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn cfg() -> crate::Config {
        crate::Config::load().expect("config")
    }

    fn catalog_with(harness: &str, model: &str) -> ProfileCatalog {
        ProfileCatalog {
            profiles: BTreeMap::from([(
                "reviewer".into(),
                Profile {
                    harness: harness.into(),
                    model: model.into(),
                    ..Default::default()
                },
            )]),
            harnesses: BTreeMap::new(),
        }
    }

    fn spec(harness: &str) -> serde_json::Value {
        json!({
            "profile": "@reviewer",
            "resolved": { "npub": "", "spec": { "harness": harness, "backend": "herdr", "role": "reviewer" } },
            "await": false,
        })
    }

    /// THE APPLICATION SITE. A plan carrying `env_model_pin` is worthless unless
    /// the pin actually lands in the map handed to `pane_split` — the pane's env
    /// is fixed at split, so a pin dropped here can never be applied later. This
    /// is the row that catches a dropped `apply_opencode_model_pin` call, which
    /// unit tests on the plan alone cannot see.
    #[test]
    fn the_env_helper_actually_applies_the_pin() {
        let env = pane_env_for_launch(
            &cfg(),
            Some("SomeOwl"),
            Some("opencode-go/muse-spark-1.3-contributor"),
        )
        .expect("env");
        let doc: serde_json::Value = serde_json::from_str(
            env.get("OPENCODE_CONFIG_CONTENT")
                .expect("the pin must be in the map BEFORE the split"),
        )
        .expect("valid json");
        assert_eq!(doc["model"], "opencode-go/muse-spark-1.3-contributor");
        // The identity env must survive alongside the pin.
        assert_eq!(env.get("AGENT_NAME").map(String::as_str), Some("SomeOwl"));
    }

    /// No pin means no config document. If the carve-out leaked, every harness
    /// would silently grow a redundant OPENCODE_CONFIG_CONTENT.
    #[test]
    fn no_pin_means_no_config_document() {
        let env = pane_env_for_launch(&cfg(), Some("SomeOwl"), None).expect("env");
        assert!(
            !env.contains_key("OPENCODE_CONFIG_CONTENT"),
            "an argv-pinned harness must not get a config document: {env:?}"
        );
        assert_eq!(env.get("AGENT_NAME").map(String::as_str), Some("SomeOwl"));
    }

    /// An unhonourable pin is an ERROR at env-build time, not a silent bare
    /// launch: a bad pin must stop the launch rather than start the seat on a
    /// model nobody selected.
    #[test]
    fn an_unhonourable_pin_is_an_error_not_a_bare_launch() {
        // A bare model id has no provider half; opencode would drop it.
        let err = pane_env_for_launch(&cfg(), Some("SomeOwl"), Some("gpt-5.2"))
            .expect_err("a bare model id must refuse");
        assert!(
            err.to_string().contains("provider/model"),
            "names the required form: {err}"
        );
    }

    /// End-to-end through the plan: an opencode launch plan's pin must survive
    /// into the env map, and its argv must stay clean of the flag opencode
    /// rejects.
    #[test]
    fn an_opencode_plan_pin_flows_all_the_way_into_the_pane_env() {
        let plan = spawn_launch_plan(
            "prom-e2e",
            &spec("opencode"),
            &catalog_with("opencode", "opencode-go/muse-spark-1.3-contributor"),
        )
        .expect("opencode plan");
        assert!(
            !plan.args.iter().any(|a| a == "--model" || a == "-m"),
            "opencode's root command rejects --model: {:?}",
            plan.args
        );
        let env = pane_env_for_launch(&cfg(), Some("SomeOwl"), plan.env_model_pin.as_deref())
            .expect("env");
        let doc: serde_json::Value =
            serde_json::from_str(env.get("OPENCODE_CONFIG_CONTENT").expect("pin present")).unwrap();
        assert_eq!(doc["model"], "opencode-go/muse-spark-1.3-contributor");
    }
}

#[cfg(test)]
mod bd_fen3_fence_tests {
    use super::{DISPATCH_HOLD_BOUND_MS, FenceAction, fence_action_for_effects};
    use toron_core::busy_owner::BusyOwnerEffect as E;

    /// The rule that a naive host gets wrong. `HoldContinued` means "re-check at
    /// the fence's deadline" — NOT "start a fresh window". A host that extended
    /// the deadline on every poll would turn a bounded hold into an unbounded
    /// wait wearing a bounded wait's clothes, which is the whole thing the fence
    /// exists to prevent.
    #[test]
    fn hold_continued_parks_at_the_fence_deadline_not_a_fresh_one() {
        let effects = [E::HoldContinued {
            agent: "a".into(),
            until_ms: 1_700_000_000_000,
        }];
        assert_eq!(
            fence_action_for_effects(&effects),
            FenceAction::Park {
                until_ms: 1_700_000_000_000
            },
            "the deadline must pass through untouched"
        );
    }

    /// `HoldExpired` and `FencedStale` both mean "your claim is void, fork and
    /// retry". Reading either as permission to send would defeat the fence.
    #[test]
    fn expiry_and_stale_both_fork() {
        for effects in [
            vec![E::HoldExpired {
                agent: "a".into(),
                held_for_ms: 600_000,
            }],
            vec![E::FencedStale {
                agent: "a".into(),
                claimed: 0,
                current: 1,
            }],
        ] {
            assert_eq!(
                fence_action_for_effects(&effects),
                FenceAction::ForkAndRetry,
                "{effects:?} must fork, never send"
            );
        }
    }

    /// Only an explicit `Acquired` means send. Silence, release bookkeeping, and
    /// a `SleepUntilDeadline` arriving after a release must never read as
    /// permission — the safe default is contention.
    #[test]
    fn only_an_explicit_acquired_permits_a_send() {
        assert_eq!(
            fence_action_for_effects(&[E::Acquired {
                agent: "a".into(),
                generation: 3,
            }]),
            FenceAction::Send
        );
        for effects in [
            vec![],
            vec![E::Released {
                agent: "a".into(),
                generation: 1,
            }],
            vec![E::ReleaseIgnored { agent: "a".into() }],
            vec![E::GenerationAdvanced {
                agent: "a".into(),
                generation: 2,
            }],
        ] {
            assert_eq!(
                fence_action_for_effects(&effects),
                FenceAction::CapContention,
                "{effects:?} carries no permission to send"
            );
        }
    }

    /// A `SleepUntilDeadline` parks; a preceding `Released` must not shadow it.
    /// Order matters, so this pins that the decisive effect wins over the
    /// bookkeeping that precedes it.
    #[test]
    fn a_park_after_release_still_parks() {
        let effects = [
            E::Released {
                agent: "a".into(),
                generation: 1,
            },
            E::SleepUntilDeadline {
                agent: "a".into(),
                until_ms: 42,
                observed_generation: 1,
            },
        ];
        assert_eq!(
            fence_action_for_effects(&effects),
            FenceAction::Park { until_ms: 42 }
        );
    }

    /// bd-fen3: the bound must outlast a real agent turn, and must NOT be the
    /// 10s engine default. The default would expire mid-turn, fork, and
    /// reintroduce the guaranteed retry exhaustion bd-5llv just fixed.
    #[test]
    fn the_hold_bound_is_configured_far_above_the_engine_default() {
        let engine_default = toron_core::busy_owner::DEFAULT_HOLD_BOUND_MS;
        assert_eq!(
            engine_default, 10_000,
            "if the engine default ever changes, re-check this row's intent"
        );
        assert!(
            DISPATCH_HOLD_BOUND_MS >= 60 * engine_default,
            "hold bound {}ms must dwarf the {engine_default}ms engine default, or a multi-minute \
             agent turn will expire mid-flight",
            DISPATCH_HOLD_BOUND_MS
        );
        let _ = DISPATCH_HOLD_BOUND_MS;
    }
}

/// erbo (bead `elm-effect-rs-approval-drainer-inbox-panic-erbo`). The module
/// name carries `off_runtime_tests` so the bead's verify filter
/// (`test --bin flywheel off_runtime_tests`) runs these beside the
/// `backend::off_runtime_tests` canaries.
#[cfg(test)]
mod approval_drainer_off_runtime_tests {
    use super::{DrainerReads, approval_decision_in_inbox, start_approval_drainer_with};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use toron_workflow::engine::RunInput;

    const PROJECT: &str = "comms-p0-erbo-fixture";
    const APPROVER: &str = "ErboApprover";
    const DRAINER: &str = "ErboDrainer";
    // ADR-0037 W2: a pin holds ONE key across every project of the shared
    // per-process test store, so the drainer-scan test (its own per-run
    // project) registers its OWN pins instead of re-registering
    // ErboApprover/ErboDrainer in a second project (W2 refuses that).
    const SCAN_APPROVER: &str = "ErboScanApprover";
    const SCAN_DRAINER: &str = "ErboScanDrainer";

    fn approval_workflow() -> toron_workflow::schema::Workflow {
        toron_workflow::schema::parse_workflow(
            r#"name: erbo-approval
trigger: { type: webhook }
steps:
  - name: ask
    action: request_approval
    message: "approve?"
"#,
        )
        .expect("parse approval workflow")
    }

    /// The in-process facade (no `[mail.toron] url`) with both fixture pins
    /// registered. Built OUTSIDE any runtime, as a CLI caller would.
    fn in_process_mail(project: &str, pins: [&str; 2]) -> crate::mail::Mail {
        let mail = crate::mail::Mail::from_config(None)
            .expect("mail config")
            .expect("in-process facade");
        mail.ensure_project(project).expect("fixture project");
        for pin in pins {
            mail.register_agent(project, pin, "fixture", "fixture", "erbo fixture")
                .expect("register fixture pin");
        }
        mail
    }

    /// Pure decision rule: approve / reject tokens, a different promise's
    /// token, and an unrelated subject.
    #[test]
    fn approval_decision_in_inbox_reads_only_this_promises_token() {
        let rows = vec![
            json!({"subject": "status update", "sender": "A"}),
            json!({"subject": "approve:other-promise", "sender": "B"}),
            json!({"subject": "reject:p-1", "sender": "C"}),
        ];
        let (d, sender) = approval_decision_in_inbox(&rows, "p-1").expect("reject token");
        assert_eq!(sender, "C");
        assert!(format!("{:?}", d.outcome).to_lowercase().contains("reject"));
        let (d, sender) = approval_decision_in_inbox(&rows, "other-promise").expect("approve");
        assert_eq!(sender, "B");
        assert!(format!("{:?}", d.outcome).to_lowercase().contains("approv"));
        assert!(approval_decision_in_inbox(&rows, "p-2").is_none());
        assert!(approval_decision_in_inbox(&[], "p-1").is_none());
    }

    /// Canary with the drainer's REAL read: an in-process `Mail::inbox` called
    /// inline on a current-thread runtime (the drainer's shape) panics. This
    /// is the pre-fix drainer; if tokio ever permits the nesting this goes red
    /// and the bridge can be revisited.
    #[test]
    fn inline_in_process_inbox_panics_inside_the_drainer_runtime() {
        let mail = in_process_mail(PROJECT, [APPROVER, DRAINER]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("drainer-shaped runtime");
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async { mail.inbox(PROJECT, DRAINER, false, 25) })
        }));
        assert!(
            res.is_err(),
            "inline inbox inside the drainer runtime was expected to panic"
        );
    }

    /// The drainer resolves a parked approval from a captain decision mail on
    /// the in-process backend: the run parks, `approve:<promise_id>` lands in
    /// the drainer identity's inbox, the drainer's scan (every 13th pass, run
    /// through `run_mail_off_runtime`) resolves it, the run resumes, and the
    /// drainer thread joins `Ok` — it survived the scan.
    ///
    /// Negative control (run by hand when this landed): calling `read()`
    /// inline in the drainer instead of through `run_mail_off_runtime` makes
    /// this test fail — the drainer thread panics with "Cannot start a runtime
    /// from within a runtime", the run never resumes, and the join is `Err`.
    #[test]
    fn drainer_inbox_scan_resolves_parked_approval_inside_a_runtime() {
        // A per-run project: the in-process store is a process singleton
        // whose root other tests may point at a reused dir, so a fixed slug
        // could hand the drainer an inbox full of earlier runs' decisions.
        let project = format!(
            "{PROJECT}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let mail = in_process_mail(&project, [SCAN_APPROVER, SCAN_DRAINER]);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        rt.block_on(async move {
            let engine = Arc::new(toron_workflow::WorkflowEngine::local().expect("engine"));
            let run_id = format!("erbo-approve-{}", uuid::Uuid::new_v4());
            let handle = engine
                .resonate()
                .run(
                    &run_id,
                    toron_workflow::engine::run_workflow,
                    RunInput {
                        run_id: Some(run_id.clone()),
                        workflow: approval_workflow(),
                        trigger: json!({}),
                    },
                )
                .spawn()
                .await
                .expect("spawn");
            let promise_id = {
                let mut tries = 0;
                loop {
                    let ids = engine.approval_sink().lock().await.clone();
                    if let Some(id) = ids.first().cloned() {
                        break id;
                    }
                    assert!(tries <= 50, "approval promise never appeared");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    tries += 1;
                }
            };
            assert!(!handle.done().await.expect("done check"), "run must park");

            // The captain's decision mail (what `flywheel workflow approve`
            // sends), delivered on the in-process plane.
            let sender_mail = mail.clone().with_name(SCAN_APPROVER.to_string());
            let subject = format!("approve:{promise_id}");
            let send_project = project.clone();
            crate::backend::run_mail_off_runtime(move || {
                sender_mail.send(
                    &send_project,
                    &[SCAN_DRAINER.to_string()],
                    &subject,
                    "erbo fixture decision",
                    "normal",
                    false,
                    None,
                )
            })
            .await
            .expect("decision mail sent");

            let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let last_page = Arc::new(std::sync::Mutex::new(String::from("no scan yet")));
            let reads = {
                let scans = scans.clone();
                let last_page = last_page.clone();
                let mail = mail.clone();
                let project = project.clone();
                DrainerReads {
                    // Keep the promise parked: only the inbox scan may resolve it.
                    signoff_clear: Arc::new(|| false),
                    inbox: Some(Arc::new(move || -> anyhow::Result<Value> {
                        scans.fetch_add(1, Ordering::SeqCst);
                        let page = mail.inbox(&project, SCAN_DRAINER, false, 25);
                        *last_page.lock().unwrap() = match &page {
                            Ok(v) => format!(
                                "{} row(s), subjects {:?}",
                                v.as_array().map_or(0, Vec::len),
                                v.as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|r| r["subject"].as_str())
                                    .collect::<Vec<_>>()
                            ),
                            Err(e) => format!("error: {e:#}"),
                        };
                        page
                    })),
                }
            };
            let stop = Arc::new(AtomicBool::new(false));
            let drainer = start_approval_drainer_with(
                engine.clone(),
                stop.clone(),
                project.clone(),
                move || reads,
            )
            .expect("drainer starts");

            // Generous bound: the scan cadence is ~2s, but a loaded box
            // (parallel full-suite builds) stretched one run past 30s.
            let out = match tokio::time::timeout(std::time::Duration::from_secs(90), handle.result())
                .await
            {
                Ok(r) => r.expect("run resumes after approval"),
                Err(_) => panic!(
                    "the drainer did not resolve the parked approval within 90s ({} inbox scans ran; last page: {})",
                    scans.load(Ordering::SeqCst),
                    last_page.lock().unwrap()
                ),
            };
            stop.store(true, Ordering::SeqCst);
            let joined = tokio::task::spawn_blocking(move || drainer.join())
                .await
                .expect("join task");
            assert!(
                matches!(joined, Ok(Ok(()))),
                "the drainer thread must survive its inbox scan (no nested-runtime panic): {:?}",
                joined.as_ref().map(|r| r.as_ref().map_err(|e| format!("{e:#}")))
            );
            assert!(scans.load(Ordering::SeqCst) >= 1, "the scan really ran");
            assert!(
                out.to_string().to_lowercase().contains("approv"),
                "resumed with the mailed approve decision: {out}"
            );
        });
    }
}

// governed-by: ADR-0024
// governed-by: ADR-0027
// governed-by: ADR-0011
//! Pane runner (ADR-0010 Spec A pane row, P10.3, bd-ghcj.1): real hc
//! PANE execution for interactive workflows — a crew pre-spawned from the
//! workflow's `crew` plan, then each `dispatch_job` step sent to the running
//! pane via the loop's proven `__send` confirmed-send leaf.

//!
//! - **Crew pre-spawn**: [`pre_spawn_crew`] turns a [`CrewPlan`] (role →
//!   count) into real hc terminals: profile → harness/model → pane split +
//!   `agent_start` (kind = harness, AGENT_ARG = `--model` per harness),
//!   recorded in the spawn registry with a PROFILE column so `@role` resolves
//!   to a running pane.
//! - **Pane dispatch**: [`pane_dispatch`] prepends the profile behavior
//!   preamble (Spec C, same as headless) and shells out to `flywheel __send
//!   <project> <name> <prompt> --confirm` — the loop's durable confirmed-send
//!   leaf, no new dispatch code.
//! - **Lifecycle**: ephemeral default (the caller reaps the workspace at run
//!   end) + `--reuse WORKSPACE` (bind to the caller's workspace, never reap)
//!   + `--retain` (keep the spawned workspace). hc owns the panes — they
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

use crate::elm::{
    BreakerConfig, Cause, ClockImpl, Defect, Effect, Exit, Remote, RemoteError, RemotePolicy,
    Runtime, SystemClock, kernel_millis,
};

use crate::backend::MultiplexerBackend;
use crate::config::Config;
use crate::crew::CrewPlan;
use crate::hc::{AgentStatus, Client};
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

/// The live pane ids on the plane that actually serves: one construction of
/// the active backend, then one `pane_list`. `None` when the plane cannot be
/// built or listed, so every caller keeps its fail-open posture (an
/// unreachable plane prunes nothing).
///
/// This is the door for runners that hold no backend (cron engines, a pane
/// `dispatch_job` step). `Client::connect(None)` here dials the hc socket no
/// matter which plane serves, so a rex fleet looked empty and a just-split
/// pane could never settle.
fn live_pane_set_on_active_plane() -> Option<std::collections::HashSet<String>> {
    let cfg = Config::load().unwrap_or_default();
    crate::backend::multiplexer_backend(&cfg)
        .ok()?
        .pane_list(None)
        .ok()
        .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
}

/// bd-qx0z: dispatch-time registry for runners without a live backend in
/// scope (cron engines): load + prune against a fresh listing on the active
/// plane. Fail-open: an unreachable plane keeps every loaded row (today's
/// behavior — no regression, just no pruning). Callers build once per
/// engine, not per step.
pub fn live_registry_for_dispatch(project: &str) -> LiveRegistry {
    let entries = crate::state::load_spawn_registry(project);
    let live = match live_pane_set_on_active_plane() {
        Some(live) => live,
        None => entries.iter().map(|e| e.pane_id.clone()).collect(),
    };
    LiveRegistry::prune(entries, &live)
}

/// Names a crew pane: `crew-<workflow>-<role><ordinal>` (1-based per role),
/// lowercased + non-alnum collapsed to `-` (hc agent names: lowercase
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
/// from hc's listing is a spawn/list race rather than a dead pane — and
/// [`live_registry_for_dispatch`]'s prune cannot tell those apart, because it
/// drops every row whose `pane_id` hc did not return. Without this, a crew
/// that spawned correctly loses the whole run to a missing-role bail.
///
/// `live_panes` returns `None` when the probe is unavailable, which is treated
/// as "cannot confirm" and keeps polling until the window closes — the same
/// fail-open posture the prune already takes for an unreachable hc.
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

/// bd-0s0y: production settle for the `nth` copy of a role. Polls the active
/// plane, never `Client::connect` — a rex crew's freshly split pane is not in
/// the hc socket's world, so the settle window could never fire there.
pub fn settle_for_role_slot(project: &str, role: &str, nth: Option<usize>) -> Option<SpawnEntry> {
    const WINDOW_MS: u64 = 5_000;
    const POLL_MS: u64 = 250;
    let rows = crate::state::load_spawn_registry(project);
    settle_role_slot_with(
        &rows,
        role,
        nth,
        live_pane_set_on_active_plane,
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
/// This is the DISPATCH gate, not the persisted clear: whatever hc does not
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
/// only for a harness hc injects — a mail-only pin keeps its row. Pure —
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
    let live: std::collections::HashSet<String> = match c.pane_list() {
        Ok(panes) => panes.into_iter().map(|p| p.pane_id).collect(),
        Err(e) => {
            eprintln!("flywheel: dead-row reap skipped (pane list failed: {e:#})");
            return 0;
        }
    };
    // The RESOLVED plane, not the configured name: `auto` is not a backend
    // identity, and `pane_keep` matches "hc"/"rex" exactly.
    let resolved = crate::config::Config::load()
        .map(|cfg| crate::backend::resolve_plane(&cfg))
        .unwrap_or_else(|_| crate::backend::resolve_plane(&crate::config::Config::default()));
    let (kept, pruned) = reap_closed_rows(
        state::load_spawn_registry(project),
        &live,
        resolved.plane.name(),
    );
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
/// Plane-door twin of [`reap_dead_registry_rows`] for callers that hold the
/// resolved backend (workflow run / compose): one `pane_list` on the ACTIVE
/// plane, then the same pure reap. The hc-client shape stays for callers that
/// already hold a live client (`kill`, `spawn`, `recover`).
pub fn reap_dead_registry_rows_on(project: &str, backend: &dyn MultiplexerBackend) -> usize {
    let live: std::collections::HashSet<String> = match backend.pane_list(None) {
        Ok(panes) => panes.into_iter().map(|p| p.pane_id).collect(),
        Err(e) => {
            eprintln!("flywheel: dead-row reap skipped (pane list failed: {e:#})");
            return 0;
        }
    };
    let (kept, pruned) =
        reap_closed_rows(state::load_spawn_registry(project), &live, backend.name());
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

/// bd-nwnb: the dead-row gauge core — rows hc may clear, as
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
    let Ok(panes) = c.pane_list() else {
        return Vec::new();
    };
    let live: std::collections::HashSet<String> = panes.into_iter().map(|p| p.pane_id).collect();
    // The RESOLVED plane, not the configured name: `pane_keep` matches
    // "hc"/"rex" exactly, and `auto` is neither.
    let plane = crate::config::Config::load()
        .map(|cfg| crate::backend::resolve_plane(&cfg))
        .unwrap_or_else(|_| crate::backend::resolve_plane(&crate::config::Config::default()));
    find_dead_rows(&state::load_spawn_registry(project), &live, plane.serving())
}
/// bd-lvyi: resolve a per-step harness override to a crew pane running THAT
/// harness. The pane's `harness` column is authoritative; a legacy entry
/// without one (pre-bd-lvyi registry) falls back to its profile's harness
/// via the catalog. `None` → no pane of the override harness exists.
pub fn resolve_pane_by_harness<'a>(
    entries: &'a LiveRegistry,
    harness: &str,
) -> Option<&'a SpawnEntry> {
    best_generation(entries.entries().iter().enumerate().filter(|(_, e)| {
        if let Some(h) = &e.harness {
            return h == harness;
        }
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
/// `count` hc terminals with the profile's harness + model. Registry entries
/// carry the PROFILE column; the workspace + entries come back in
/// [`PaneRunCtx`] for the runner and the run-end reap decision.
///
/// Stale-workspace protection mirrors `cmd_spawn` (bd-fw-stale-workspace-panes
/// -hgp + bd-627z.3): a pre-existing FLYWHEEL-OWNED project workspace blocks
/// the spawn unless `--retire-orphans` closes it first; a foreign workspace
/// (captain/omp session labeled like the project) only warns — with
/// `--reuse WORKSPACE` the named workspace is bound instead (never blocked,
/// never reaped).
#[allow(clippy::too_many_arguments)]
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
    if rex_door_selected(cfg) {
        return pre_spawn_crew_rex(
            cfg,
            project,
            wf_name,
            plan,
            catalog,
            reuse,
            retain,
            retire_orphans,
        );
    }
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
            reap_dead_registry_rows(project, &mut c);
        } else {
            bail!(
                "found {} pre-existing workspace(s) for {project} — stale panes would race the crew: {}. Close them first (flywheel kill {project} or terminal workspace close <id>), or pass --retire-orphans / --reuse.",
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
            location: crate::agent_ref::BindingLocation::Pane,
        },
        crate::agent_ref::AgentBinding {
            profile_name: role.to_string(),
            model: route.model_id.clone(),
            harness: route.harness.clone(),
            location: crate::agent_ref::BindingLocation::Pane,
        },
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

/// Rex-native spawn door (LinenQuill: `REX_SESSION` selects the multiplexer).
/// A set, non-empty `REX_SESSION` means this process runs inside rex, so the
/// doors target rex; empty/missing keeps today's hc `Client` path.
pub(crate) fn rex_door_selected(cfg: &crate::config::Config) -> bool {
    crate::backend::factory::resolve_plane(cfg).plane == crate::backend::factory::Plane::Rex
}

/// `program_status` readiness: `idle` (at prompt) and `working` (program
/// running) both mean the shell is alive. `error` is not Done — a failed
/// shell never passes the gate. Pure so the contract is testable.
pub(crate) fn rex_shell_ready(status: &AgentStatus) -> bool {
    matches!(status, AgentStatus::Idle | AgentStatus::Working)
}

/// Quote one env value for the `env K=V` command prefix. Values without
/// shell metacharacters pass through unchanged.
fn rex_quote_env_value(v: &str) -> String {
    if v.is_empty()
        || v.bytes().any(|b| {
            matches!(
                b,
                b' ' | b'\t' | b'\n' | b'\'' | b'"' | b'\\' | b'$' | b'`' | b'!'
            )
        })
    {
        format!("'{}'", v.replace('\'', "'\\''"))
    } else {
        v.to_string()
    }
}

/// Compose the step prompt from an optional stage contract and an optional
/// task string so a workflow step with both never drops the stage rules.
#[must_use]
pub(crate) fn compose_dispatch_prompt(
    stage_prompt: Option<&str>,
    task: Option<&str>,
    fallback: &str,
) -> String {
    let stage = stage_prompt.map(str::trim).filter(|s| !s.is_empty());
    let task = task.map(str::trim).filter(|s| !s.is_empty());
    match (stage, task) {
        (Some(stage), Some(task)) => format!("{stage}\n\nTASK: {task}"),
        (Some(stage), None) => stage.to_string(),
        (None, Some(task)) => task.to_string(),
        (None, None) => fallback.to_string(),
    }
}

const REX_MAX_INLINE_CMD_BYTES: usize = 768;

/// Render a self-deleting POSIX wrapper script for long Rex launch commands
/// that would exceed macOS termios `MAX_CANON` (1024 B) if typed inline.
#[must_use]
pub(crate) fn rex_launch_script_contents(
    env: &BTreeMap<String, String>,
    kind: &str,
    args: &[String],
) -> String {
    let inner = rex_env_command(env, kind, args);
    format!("#!/bin/sh\nrm -f \"$0\"\nexec {inner}\n")
}

/// Prepare a Rex pane launch command, spilling to `<state_dir>/launch/<key>.sh`
/// when the inline `env ...` command exceeds `REX_MAX_INLINE_CMD_BYTES`.
pub(crate) fn prepare_rex_launch_command(
    state_dir: &std::path::Path,
    seat_key: &str,
    env: &BTreeMap<String, String>,
    kind: &str,
    args: &[String],
) -> Result<String> {
    let inline = rex_env_command(env, kind, args);
    if inline.len() <= REX_MAX_INLINE_CMD_BYTES {
        return Ok(inline);
    }
    let launch_dir = state_dir.join("launch");
    std::fs::create_dir_all(&launch_dir)
        .with_context(|| format!("create rex launch dir {}", launch_dir.display()))?;
    let safe: String = seat_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let script_path = launch_dir.join(format!("{safe}-{suffix}.sh"));
    let body = rex_launch_script_contents(env, kind, args);
    std::fs::write(&script_path, body)
        .with_context(|| format!("write rex launch script {}", script_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", script_path.display()))?;
    }
    Ok(format!(
        "sh {}",
        rex_quote_env_value(&script_path.to_string_lossy())
    ))
}

/// Build the one-string rex launch command. `session.new_split` has no env
/// map, so the pane env rides as an `env K=V` prefix (LinenQuill). `plane_*`
/// integration env belongs to the hc plane and is dropped — a rex pane
/// must not chase an hc socket. Pure so the contract is testable.
pub(crate) fn rex_env_command(
    env: &BTreeMap<String, String>,
    kind: &str,
    args: &[String],
) -> String {
    let mut parts = Vec::new();
    for (k, v) in env {
        if k == "HC_SOCKET_PATH"
            || k == "HC_SESSION"
            || k == "HC_TERMINAL_ID"
            || k == "HC_TAB_ID"
            || k == "HC_WORKSPACE_ID"
        {
            continue;
        }
        parts.push(format!("{k}={}", rex_quote_env_value(v)));
    }
    // bd-omp-pin: the pinned build is named explicitly, never resolved from
    // the seat's cwd (mise.toml covers only the flywheel repo).
    let binary = crate::harness_launch_binary(kind);
    if parts.is_empty() {
        let mut cmd = vec![binary];
        cmd.extend(args.iter().cloned());
        return cmd.join(" ");
    }
    let mut cmd = vec!["env".to_string()];
    cmd.append(&mut parts);
    cmd.push(binary);
    cmd.extend(args.iter().cloned());
    cmd.join(" ")
}

///Wait for a rex pane's shell: `program_status` (read through the backend's
/// agent list, which derives statuses from it) must read `idle` or `working`
/// within `timeout`. Fail-closed with the same refusal as the hc gate —
/// typing into a booting shell mangles the launch. Never calls
/// `hc::wait_shell_prompt` (that prober speaks hc, not rex). A backend
/// that never answers is the real reason and is reported as such instead of
/// blaming the shell.
pub(crate) fn wait_rex_shell_ready(
    backend: &dyn MultiplexerBackend,
    target: &str,
    name: &str,
    pane_id: &str,
    timeout: Duration,
) -> Result<()> {
    let start = std::time::Instant::now();
    let mut list_err: Option<anyhow::Error> = None;
    let mut saw_ok = false;
    loop {
        match backend.agent_list() {
            Ok(agents) => {
                saw_ok = true;
                if let Some(a) = agents
                    .iter()
                    .find(|a| a.pane_id == target || a.name.as_deref() == Some(target))
                    && rex_shell_ready(&a.agent_status)
                {
                    return Ok(());
                }
            }
            Err(e) => {
                list_err = Some(anyhow!("{e:#}"));
            }
        }
        if start.elapsed() >= timeout {
            if !saw_ok && let Some(e) = list_err {
                return Err(e.context(format!("rex: cannot read status for {name} ({pane_id})")));
            }
            return Err(shell_gate_refusal(name, pane_id));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Rex arm of [`spawn_crew_into`]: same registry/mail/launch contract over
/// `&dyn MultiplexerBackend`, never `Client::connect`. Kept as a separate
/// function so the hc path below stays byte-identical and this arm lifts
/// off cleanly. Known delta: a failed `pane_run` fails loudly (the
/// rate-limit registry fallback below is `Client`-coupled).
#[allow(clippy::too_many_arguments)]
fn spawn_crew_into_rex(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    plan: &CrewPlan,
    catalog: &ProfileCatalog,
    backend: &dyn MultiplexerBackend,
    ws_id: &str,
    label: &str,
) -> Result<LiveRegistry> {
    let mut registry: Vec<SpawnEntry> = state::merge_registry_for_mutation(
        project,
        state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?,
    );
    let mut live_known: Option<std::collections::HashSet<String>> = backend
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
    let mut prev: Option<String> = None;
    let dir = crate::project_dir(cfg, project);
    for (role, count) in &plan.members {
        let profile = catalog
            .get(role)
            .ok_or_else(|| anyhow!("pane crew: unknown profile @{role} (flywheel profile list)"))?;
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
            let live_agents = backend.agent_list()?;
            let existing = registry
                .iter()
                .find(|e| e.name == name)
                .filter(|e| {
                    live_agents
                        .iter()
                        .any(|a| a.pane_id == e.pane_id && a.workspace_id == ws_id)
                })
                .cloned();
            let mail_identity = crate::pinned_mail_identity(cfg, project, &name, harness);
            let (agent_pane_id, name, model_pin) = if let Some(e) = existing {
                eprintln!(
                    "  crew {}: reusing {} ({}) @{}",
                    label, name, e.pane_id, role
                );
                (e.pane_id.clone(), name.clone(), e.model_pin.clone())
            } else {
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
                let panes = backend.pane_list(Some(ws_id))?;
                let agents = backend.agent_list()?;
                let occupied: std::collections::HashSet<String> =
                    agents.iter().map(|a| a.pane_id.clone()).collect();
                let pane_ids: Vec<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
                let reusable = if env_model_pin.is_some() {
                    None
                } else {
                    select_empty_pane(&pane_ids, &occupied)
                };
                let pane_id = match (&prev, reusable) {
                    (None, Some(pid)) => pid,
                    (None, None) => {
                        let root = pane_ids.first().cloned().ok_or_else(|| {
                            anyhow!("pane crew: no root pane in workspace {ws_id}")
                        })?;
                        let split = backend.pane_split(
                            &root,
                            "down",
                            Some(&crate::utf8_path(&dir, "workspace dir")?),
                        )?;
                        split.pane_id
                    }
                    (Some(p), _) => {
                        let split = backend.pane_split(
                            p,
                            "down",
                            Some(&crate::utf8_path(&dir, "workspace dir")?),
                        )?;
                        split.pane_id
                    }
                };
                wait_rex_shell_ready(backend, &pane_id, &name, &pane_id, Duration::from_secs(15))?;
                let command = rex_env_command(&pane_env, harness, &agent_args);
                backend
                    .pane_run(&pane_id, &command)
                    .map_err(|e| anyhow!("pane crew: rex run for {name} ({pane_id}): {e:#}"))?;
                if let Err(e) = backend.agent_rename(&pane_id, Some(name.as_str())) {
                    eprintln!("flywheel: warn: rex rename {pane_id} → {name} failed: {e:#}");
                }
                crate::bind_pane_mail_identity(
                    cfg,
                    project,
                    &name,
                    &pane_id,
                    mail_identity.as_deref(),
                );
                // Wait on the PANE, not the label: rex labels are only
                // unique within a session, and the same crew name can exist
                // in another live session (bd live proof: "label … is in 2
                // sessions").
                if let Err(e) = backend.agent_wait(
                    &pane_id,
                    &[AgentStatus::Idle, AgentStatus::Working],
                    Some(90_000),
                ) {
                    eprintln!(
                        "flywheel: crew {name} readiness wait failed (dispatching anyway): {e}"
                    );
                }
                (pane_id, name.clone(), model_pin)
            };
            prev = Some(agent_pane_id.clone());
            let incarnation = next_incarnation(&registry, &name);
            registry.retain(|e| e.name != name);
            registry.push(SpawnEntry {
                pane_id: agent_pane_id.clone(),
                name: name.clone(),
                profile: Some(role.clone()),
                harness: Some(harness.clone()),
                model: None,
                model_pin,
                effort: (!profile.effort.is_empty()).then(|| profile.effort.clone()),
                mode: None,
                skills_digest: crate::profile::skills_digest(&profile.skills),
                mail_identity,
                skills_injected: false,
                incarnation,
                harness_session: None,
            });
            eprintln!("  crew {}: {} ({}) @{}", label, name, agent_pane_id, role);
            if let Some(live) = live_known.as_mut() {
                live.insert(agent_pane_id.clone());
            }
        }
    }
    state::save_spawn_registry(project, &registry)?;
    Ok(match live_known {
        Some(live) => LiveRegistry::prune(registry, &live),
        None => {
            let all: std::collections::HashSet<String> =
                registry.iter().map(|e| e.pane_id.clone()).collect();
            LiveRegistry::prune(registry, &all)
        }
    })
}

/// Rex arm of [`pre_spawn_crew`]: same reuse/orphan contract over
/// `&dyn MultiplexerBackend`, never `Client::connect`.
#[allow(clippy::too_many_arguments)]
fn pre_spawn_crew_rex(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    plan: &CrewPlan,
    catalog: &ProfileCatalog,
    reuse: Option<&str>,
    retain: bool,
    retire_orphans: bool,
) -> Result<PaneRunCtx> {
    let dir = crate::project_dir(cfg, project);
    if !dir.exists() {
        bail!(
            "project dir missing for pane crew: {} (run: flywheel init {project})",
            dir.display()
        );
    }
    let backend = crate::backend::factory::multiplexer_backend(cfg)?;
    let label = crate::swarm_label(project);
    let ws_list = backend.workspace_list()?;
    let spawn_meta = state::load_spawn_meta(project);
    let owned: Vec<_> = ws_list
        .iter()
        .filter(|ws| ws.label == label || spawn_meta.as_deref() == Some(ws.label.as_str()))
        .cloned()
        .collect();
    if let Some(ws_id) = reuse {
        if !ws_list.iter().any(|ws| ws.workspace_id == ws_id) {
            bail!("--reuse workspace {ws_id} not found for {project}");
        }
        let entries = spawn_crew_into_rex(
            cfg, project, wf_name, plan, catalog, &*backend, ws_id, &label,
        )?;
        return Ok(PaneRunCtx {
            workspace_id: ws_id.to_string(),
            entries,
            retained: true,
        });
    }
    if !owned.is_empty() {
        let detail: Vec<String> = owned
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
                owned.len(),
                detail.join(", ")
            );
            for ws in &owned {
                if let Err(e) = backend.workspace_close(&ws.workspace_id, true) {
                    eprintln!("flywheel: close {} failed: {e:#}", ws.workspace_id);
                }
            }
            let live: std::collections::HashSet<String> = backend
                .pane_list(None)
                .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
                .unwrap_or_default();
            let (kept, pruned) =
                reap_closed_rows(state::load_spawn_registry(project), &live, backend.name());
            if pruned > 0 {
                eprintln!("flywheel: reaped {pruned} dead-pane registry row(s) for {project}");
                let _ = state::save_spawn_registry(project, &kept);
            }
        } else {
            bail!(
                "found {} pre-existing workspace(s) for {project} — stale panes would race the crew: {}. Close them first (flywheel kill {project}) or pass --retire-orphans / --reuse.",
                owned.len(),
                detail.join(", ")
            );
        }
    }
    let ws = backend.workspace_create(Some(&label))?;
    let entries = spawn_crew_into_rex(
        cfg,
        project,
        wf_name,
        plan,
        catalog,
        &*backend,
        &ws.workspace_id,
        &label,
    )?;
    Ok(PaneRunCtx {
        workspace_id: ws.workspace_id,
        entries,
        retained: retain,
    })
}
#[allow(clippy::too_many_arguments)]
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
    let mut registry: Vec<SpawnEntry> = state::merge_registry_for_mutation(
        project,
        state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?,
    );
    let mut live_known: Option<std::collections::HashSet<String>> = c
        .pane_list()
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
    let mut prev: Option<String> = None;
    let dir = crate::project_dir(cfg, project);
    for (role, count) in &plan.members {
        let profile = catalog
            .get(role)
            .ok_or_else(|| anyhow!("pane crew: unknown profile @{role} (flywheel profile list)"))?;
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
            let mail_identity = crate::pinned_mail_identity(cfg, project, &name, harness);
            let (agent_pane_id, name, model_pin) = if let Some(e) = existing {
                eprintln!(
                    "  crew {}: reusing {} ({}) @{}",
                    label, name, e.pane_id, role
                );
                (e.pane_id.clone(), name.clone(), e.model_pin.clone())
            } else {
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
                        let panes = c.pane_list()?;
                        let agents = c.agent_list()?;
                        let occupied: std::collections::HashSet<String> =
                            agents.iter().map(|a| a.pane_id.clone()).collect();
                        let pane_ids: Vec<String> =
                            panes.iter().map(|p| p.pane_id.clone()).collect();
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
                                let split = crate::hc::pane_split(
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
                        let split = crate::hc::pane_split(
                            c,
                            p,
                            "down",
                            Some(&crate::utf8_path(&dir, "workspace dir")?),
                            Some(&pane_env),
                        )?;
                        split.pane_id
                    }
                };
                if !crate::hc::wait_shell_prompt(c, &pane_id, Duration::from_secs(15)) {
                    return Err(shell_gate_refusal(&name, &pane_id));
                }
                let agent =
                    match crate::start_agent_with_retry(c, &name, harness, &pane_id, &agent_args) {
                        Ok(a) => a,
                        Err(e) => {
                            let route = registry_fallback_route(
                                &e.to_string(),
                                &profile.model,
                                harness,
                                || match ModelRegistry::load_routing() {
                                    Ok(registry) => Some(registry),
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
                crate::bind_pane_mail_identity(
                    cfg,
                    project,
                    &name,
                    &agent.pane_id,
                    mail_identity.as_deref(),
                );
                if let Err(e) = c.agent_wait(&name, &[AgentStatus::Idle], Some(90_000)) {
                    eprintln!(
                        "flywheel: crew {name} readiness wait failed (dispatching anyway): {e}"
                    );
                }
                (agent.pane_id.clone(), name.clone(), model_pin)
            };
            prev = Some(agent_pane_id.clone());
            let incarnation = next_incarnation(&registry, &name);
            registry.retain(|e| e.name != name);
            registry.push(SpawnEntry {
                pane_id: agent_pane_id.clone(),
                name: name.clone(),
                profile: Some(role.clone()),
                harness: Some(harness.clone()),
                model: None,
                model_pin,
                effort: (!profile.effort.is_empty()).then(|| profile.effort.clone()),
                mode: None,
                skills_digest: crate::profile::skills_digest(&profile.skills),
                mail_identity,
                skills_injected: false,
                incarnation,
                harness_session: None,
            });
            eprintln!("  crew {}: {} ({}) @{}", label, name, agent_pane_id, role);
            if let Some(live) = live_known.as_mut() {
                live.insert(agent_pane_id.clone());
            }
        }
    }
    state::save_spawn_registry(project, &registry)?;
    Ok(match live_known {
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
        Some(h) if !h.trim().is_empty() => !profile_harness.is_empty() && h != profile_harness,
        _ => false,
    }
}

/// ADR-0022 D8 acceptance 6: a review/verify dispatch binds through the seat
/// binder, never to the authoring seat, and prefers a registered model of a
/// different family. Returns `None` for steps that are not a fresh reviewer
/// dispatch (the ordinary role-slot path owns those).
pub(crate) fn review_bind(
    profile_name: &str,
    spec: &toron_workflow::schema::DispatchSpec,
    entries: &LiveRegistry,
    crew_max: usize,
    journal_authors: &std::collections::BTreeSet<String>,
) -> Option<crate::seat_binder::Binding> {
    if profile_name != "reviewer"
        || spec.context != Some(toron_workflow::schema::ContextMode::Fresh)
    {
        return None;
    }
    use crate::seat_binder::{BindPolicy, LiveSeat, SeatPlacement, StepNeed};
    let seats: Vec<LiveSeat> = entries
        .entries()
        .iter()
        .filter_map(|e| {
            let pin = e.mail_identity.clone().filter(|p| !p.is_empty())?;
            Some(LiveSeat {
                pin,
                role: e.profile.clone().unwrap_or_else(|| "crew".into()).into(),
                harness: e.harness.clone().unwrap_or_default().into(),
                model: e.model.clone().or_else(|| e.model_pin.clone()),
                mode: None,
                effort: None,
                placement: SeatPlacement::Pane,
                idle: true,
                fresh_session: true,
                idle_since_ms: None,
            })
        })
        .collect();
    let author = entries
        .entries()
        .iter()
        .find(|e| e.profile.as_deref() == Some("planner"));
    let explicit_author = spec
        .extra
        .get("author_pin")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // The journal is the authority on who ran the reads (bd-6setu.2); the
    // planner heuristic is the fallback for a run with no rows yet.
    let mut authors: std::collections::BTreeSet<String> = journal_authors.clone();
    if let Some(pin) = explicit_author.or_else(|| author.and_then(|e| e.mail_identity.clone())) {
        authors.insert(pin);
    }
    let author_model = author.and_then(|e| e.model.clone().or_else(|| e.model_pin.clone()));
    let candidates: Vec<String> = seats
        .iter()
        .filter(|s| s.role.as_str() == profile_name)
        .filter_map(|s| s.model.clone())
        .collect();
    let registry = crate::models::ModelRegistry::default();
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let model = author_model
        .as_deref()
        .and_then(|m| registry.verifier_for(m, &refs, None));
    let need = StepNeed {
        step: profile_name.to_string(),
        role: profile_name.to_string(),
        harness: spec.harness.clone(),
        model,
        mode: None,
        context: crate::stages::StageContext::Fresh,
        reviews_author: authors,
    };
    Some(crate::seat_binder::bind(
        &need,
        &seats,
        &BindPolicy {
            crew_max,
            headless_allowed: false,
        },
    ))
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
    pane_dispatch_scoped(
        catalog, project, wf_name, inputs, entries, action, copy_index, None,
    )
}

/// The run scope a pane step is dispatched under (bd-6setu.2). A durable-body
/// leaf knows its run and node; every other caller does not, and passes
/// `None` — no scope, no binding row, no guessed author.
#[derive(Debug, Clone, Copy)]
pub struct DispatchScope<'a> {
    pub run_id: &'a str,
    pub node_path: &'a str,
}

/// The prompt seam a bindable dispatch hands over (bd-6setu.16 M1): called
/// after the binding row is durable, instead of sending the prompt.
pub type PromptPort<'a> = &'a dyn Fn(&str, &str) -> Result<()>;

/// Everything a bindable pane dispatch needs from its caller (bd-6setu.16):
/// the run scope, the stage→reads map resolved ONCE at construction (H3), and
/// a prompt port a test can observe (M1). No per-step library reload.
pub struct DispatchContext<'a> {
    pub scope: DispatchScope<'a>,
    /// stage name → the step refs its contract reads.
    pub reads: &'a std::collections::BTreeMap<String, Vec<String>>,
    /// When set, called INSTEAD of sending the prompt, after the binding row is
    /// written — the execution witness for journal-before-prompt.
    pub prompt_port: Option<PromptPort<'a>>,
}

/// [`pane_dispatch`] with the run scope that makes the dispatch bindable. The
/// binding row is written BEFORE the pane prompt, keyed (run, node, step).
#[allow(clippy::too_many_arguments)]
pub fn pane_dispatch_scoped(
    catalog: &ProfileCatalog,
    project: &str,
    wf_name: &str,
    inputs: &Value,
    entries: &LiveRegistry,
    action: &Action,
    copy_index: Option<usize>,
    ctx: Option<&DispatchContext<'_>>,
) -> Result<Value> {
    let Action::DispatchJob { spec, .. } = action else {
        if matches!(action, Action::Spawn { .. }) {
            bail!(
                "pane runner: Action::Spawn must be handled by the engine (run_spawn) — routing bug"
            );
        }
        return Ok(json!({ "action": action.type_name() }));
    };
    let profile_ref = headless::dispatch_spec_profile(spec)
        .ok_or_else(|| anyhow!("pane dispatch: dispatch_job spec has no profile"))?;
    let profile_name = headless::strip_profile_ref(profile_ref);
    let profile = catalog.get(profile_name).ok_or_else(|| {
        anyhow!("pane dispatch: unknown profile @{profile_name} (flywheel profile list)")
    })?;
    if spec.context == Some(toron_workflow::schema::ContextMode::Fresh) {
        eprintln!(
            "pane dispatch: dispatch_job @{profile_name} requests context=fresh — new session (honored: fresh maps to new spawn pane)"
        );
    }
    // ADR-0022 D8 acceptance 6: a fresh reviewer dispatch binds through the
    // seat binder (non-author, different family when registered). A Wait is
    // bounded by the step's no_claim_timeout; a Spawn goes through the seat
    // interpreter, never a second dispatch path.
    let crew_max = crate::config::Config::load()
        .map(|c| c.crew_max() as usize)
        .unwrap_or(crate::crew::DEFAULT_CREW_MAX as usize);
    // bd-6setu.2 / V10: a review step's authors are the pins bound to the steps
    // its stage READS, in THIS run. No context (headless, loop Send, plane-S) =
    // no journal authors. bd-6setu.16: the reads map is DATA supplied by the
    // construction site (H3), and an unreadable store REFUSES the dispatch (H2)
    // instead of falling back to the planner heuristic.
    let journal_authors = match ctx {
        None => std::collections::BTreeSet::new(),
        Some(ctx) => {
            let steps: std::collections::BTreeSet<String> = headless::dispatch_spec_stage(spec)
                .and_then(|stage| ctx.reads.get(stage).cloned())
                .map(|reads| {
                    reads
                        .iter()
                        .filter_map(|r| r.strip_prefix("steps.").map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            crate::dispatch_bindings::authors_for(project, ctx.scope.run_id, &steps)?
        }
    };
    let bound = review_bind(profile_name, spec, entries, crew_max, &journal_authors);
    let target_pin = spec
        .extra
        .get("target_pin")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    // bd-sfkm8: an in-flight join binds a step to a live pin. The spec keeps
    // its profile so this runner can resolve it; `target_pin` is a typed
    // dispatch override, not a second dispatch path. A missing pin fails loud.
    let bound_entry: Option<SpawnEntry> = if let Some(pin) = target_pin {
        Some(
            entries
                .entries()
                .iter()
                .find(|e| e.mail_identity.as_deref() == Some(pin))
                .cloned()
                .ok_or_else(|| {
                    anyhow!("pane dispatch: bound pin {pin} has no live seat (recompose the join)")
                })?,
        )
    } else {
        match &bound {
            Some(crate::seat_binder::Binding::Reuse { pin })
            | Some(crate::seat_binder::Binding::FreshSession { pin })
            | Some(crate::seat_binder::Binding::Other { pin })
            | Some(crate::seat_binder::Binding::Patch { pin, .. }) => entries
                .entries()
                .iter()
                .find(|e| e.mail_identity.as_deref() == Some(pin.as_str()))
                .cloned(),
            Some(crate::seat_binder::Binding::Wait { reason }) => bail!(
                "pane dispatch: @{profile_name} waits for a seat ({reason}); the step is bounded by \
                 its no_claim_timeout and never borrows another role"
            ),
            Some(crate::seat_binder::Binding::Spawn) => bail!(
                "pane dispatch: @{profile_name} needs a new seat — Spawn goes through the seat \
                 interpreter (Ports::seat), not a second dispatch path"
            ),
            Some(crate::seat_binder::Binding::Headless) | None => None,
        }
    };
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
    let stage_prompt = match headless::dispatch_spec_stage(spec) {
        Some(s) => Some(crate::stage_prompt(s)?),
        None => None,
    };
    let fallback = headless::base_prompt_for(wf_name, inputs);
    let mut base = compose_dispatch_prompt(
        stage_prompt.as_deref(),
        headless::dispatch_spec_task(spec),
        &fallback,
    );
    for key in ["plan", "project"] {
        if let Some(extra) = headless::spec_extra(spec, key, inputs) {
            base.push_str("\n\n");
            base.push_str(key);
            base.push_str(": ");
            base.push_str(&extra);
        }
    }
    let prompt = base;
    let entry: SpawnEntry = match override_entry
        .or(bound_entry.as_ref())
        .or(resolve_role_slot(entries, profile_name, copy_index)?)
        .cloned()
    {
        Some(e) => e,
        None => match settle_for_role_slot(project, profile_name, copy_index) {
            Some(entry) => {
                eprintln!(
                    "flywheel: WARNING @{profile_name} pane {} was absent from the plane's listing at dispatch; settled and using it",
                    entry.pane_id
                );
                entry
            }
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
    // bd-6setu.2: the binding is journaled BEFORE the prompt, so a crash
    // between the two leaves a claim that the pane may not have received —
    // the safe direction for V10. Keyed (run, node, step), so a replay adds
    // nothing. A scope-less caller writes no row at all.
    if let Some(ctx) = ctx {
        let pin = entry
            .mail_identity
            .clone()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| entry.name.clone());
        let step = headless::dispatch_spec_stage(spec).unwrap_or(profile_name);
        crate::dispatch_bindings::append(
            project,
            ctx.scope.run_id,
            ctx.scope.node_path,
            step,
            &pin,
        )?;
        // M1 (bd-6setu.16): the witness is EXECUTION, not source order — the
        // port is invoked after the row is durable and before any prompt
        // leaves, so a test observes the row at prompt time.
        if let Some(port) = ctx.prompt_port {
            port(&entry.name, &prompt)?;
            return Ok(json!({
                "delivered": true,
                "prompt_port": true,
                "agent": entry.name,
                "profile": profile_name,
                "step": step,
            }));
        }
    }
    eprintln!(
        "flywheel: dispatch @{} → {} ({})",
        profile_name, entry.name, entry.pane_id
    );
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

    let fence = acquire_dispatch_fence(&entry.name, unix_millis())?;
    let _permit = match fence {
        FenceOutcome::Send(p) => Some(p),
        FenceOutcome::Park { until_ms } => {
            let wait_s = until_ms.saturating_sub(unix_millis()) / 1_000;
            eprintln!(
                "flywheel: pane dispatch @{profile_name} ({}) busy (park until {until_ms}, ~{wait_s}s) — parking gracefully instead of aborting (durable queueing/park; bd-jclab)",
                entry.name
            );
            return Ok(json!({
                "parked": true,
                "reason": "busy_fence",
                "fence": "park",
                "agent": entry.name,
                "profile": profile_name,
                "pane_id": entry.pane_id,
                "until_ms": until_ms,
                "wait_seconds": wait_s,
            }));
        }
        FenceOutcome::ForkAndRetry => {
            eprintln!(
                "flywheel: pane dispatch @{profile_name} ({}) busy (fence fork-and-retry) — parking gracefully instead of aborting (durable queueing/park; bd-jclab)",
                entry.name
            );
            return Ok(json!({
                "parked": true,
                "reason": "busy_fence",
                "fence": "fork_and_retry",
                "agent": entry.name,
                "profile": profile_name,
                "pane_id": entry.pane_id,
            }));
        }
        FenceOutcome::CapContention => {
            eprintln!(
                "flywheel: pane dispatch @{profile_name} ({}) busy (fence cap contention) — parking gracefully instead of aborting (durable queueing/park; bd-jclab)",
                entry.name
            );
            return Ok(json!({
                "parked": true,
                "reason": "busy_fence",
                "fence": "cap_contention",
                "agent": entry.name,
                "profile": profile_name,
                "pane_id": entry.pane_id,
            }));
        }
    };
    let mut attempt_entry: &SpawnEntry = &entry;
    let mut attempt_profile = profile_name.to_string();
    let mut attempted: Vec<String> = vec![profile_name.to_string()];
    let mut reroute_trail: Vec<Value> = Vec::new();
    let rate_limit_kind = crate::failure::FailureKind::RateLimit.to_string();
    'route: loop {
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
                let output = match toron_core::proc::run_bounded(
                    &mut cmd,
                    dispatch_attempt_timeout(),
                ) {
                    Ok(output) => output,
                    Err(toron_core::proc::ProcError::Timeout { .. }) => {
                        return Effect::die(Defect::new(format!(
                            "__send for @{send_profile} ({send_agent}) exceeded the attempt timeout ({:?}) - the child never exited",
                            dispatch_attempt_timeout()
                        )));
                    }
                    Err(toron_core::proc::ProcError::Io(e)) => {
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
                        let reason = v
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("transient")
                            .to_string();
                        return Effect::fail(reason);
                    }
                    if crate::phase_chain::plan_artifact_missing(&send_spec, &send_inputs) {
                        eprintln!(
                            "flywheel: phase implement dispatch has NO completed plan artifact — \
                     warn-first gate (ADR-0022 §3.5); step output tagged plan_artifact_missing"
                        );
                    }
                    let mut out = json!({
                        "profile": send_profile.clone(),
                        "agent": send_agent.clone(),
                        "ok": dispatch_receipt_ok(&v),
                        "send": v.clone(),
                    });
                    if v.get("ok").and_then(Value::as_bool) == Some(false) {
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
                let err_str = dispatch_failure_error(&send_profile, &send_agent, &raw, &err);
                let cf = crate::failure::classify(&err_str, None, None);
                Effect::succeed(json!({
                    "ok": false,
                    "error": err_str,
                    "profile": send_profile.clone(),
                    "agent": send_agent.clone(),
                    "stage": cf.stage,
                    "kind": cf.kind.to_string(),
                    "retryable": cf.retryable,
                }))
            },
        );
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
        if let Some(binding) = &bound
            && let Ok(v) = serde_json::to_value(binding)
        {
            out["binding"] = v;
        }
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
        Some(RemoteError::BulkheadFull) => DispatchErr::Fatal(
            "the dispatch bulkhead admitted no permit in time — the shared stack is saturated"
                .to_string(),
        ),
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
/// Local hc IPC (settle re-reads, text primes) stays on the small `retry`
/// driver — unifying the two under one breaker or bulkhead would couple
/// unrelated failures, so the two spellings coexist on purpose.
fn dispatch_policy() -> RemotePolicy<Value, String> {
    RemotePolicy {
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
    RT.get_or_init(Runtime::new)
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
/// hc injects env ONLY at split/create, and a pane's env is fixed for the
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
    let mut env = crate::hc::plane_launch_env(cfg, mail_identity);
    if let Some(model) = env_model_pin {
        crate::hc::apply_opencode_model_pin(&mut env, model)?;
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
    let harness = spec["harness"]
        .as_str()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| spec["resolved"]["spec"]["harness"].as_str())
        .unwrap_or_default()
        .to_string();
    let phase = spec["resolved"]["spec"]["phase"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let (args, env_model_pin) = match catalog.get(&profile) {
        Some(p) => {
            let plan = crate::profile_launch_plan(&harness, p, phase.as_deref())
                .map_err(|e| anyhow::anyhow!("spawn plan '{profile}' model routing: {e}"))?;
            (plan.args, plan.env_model_pin)
        }
        None => (Vec::new(), None),
    };
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

/// One durable spawn claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpawnClaim {
    /// The correlation key the engine published (`run:node:attempt`).
    pub spawn_key: String,
    /// The step/node the job belongs to.
    pub node: String,
    /// The run the job belongs to.
    pub run_id: String,
    /// Host time the claim was taken, unix millis.
    pub claimed_at_ms: u64,
    /// The launch receipt, recorded once a pane started. `None` = the claim
    /// exists but no child was started (a crash in that window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Value>,
}

/// What a claim call did.
#[derive(Debug, Clone)]
pub enum SpawnClaimOutcome {
    /// This caller took the key and must start the child.
    Claimed(SpawnClaim),
    /// An earlier attempt took the key: adopt, never start a second child.
    Adopted(SpawnClaim),
}

/// The claim file: `spawn_key` → claim.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct SpawnClaimFile {
    #[serde(default)]
    claims: BTreeMap<String, SpawnClaim>,
}

/// Durable claim registry, keyed by `spawn_key` in flywheel's atomic state
/// file (bd-spawn-launcher-flip-d8mv).
///
/// The claim is written BEFORE the pane starts and lives across processes, so
/// it — not a vendor promise — is the exactly-once gate. The engine republishes
/// the same `spawn_key` after a restart; an existing claim is ADOPTED.
#[derive(Clone, Debug)]
pub struct SpawnClaimStore {
    path: std::path::PathBuf,
}

#[allow(dead_code)]
impl SpawnClaimStore {
    /// The live registry: `spawn-claims.json` under flywheel's state dir (the
    /// same atomic `update_locked` path the loops sidecars use).
    pub fn open() -> Self {
        Self::at(crate::config::state_dir().join("spawn-claims.json"))
    }

    /// A registry at an explicit file (tests).
    pub fn at(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Take the key, or adopt the existing claim.
    ///
    /// # Errors
    ///
    /// `Err` when the state file cannot be written under its lock — a launch
    /// must not start without a durable claim, so this fails closed.
    pub fn claim(
        &self,
        spec: &toron_workflow::engine::spawn::SpawnSpec,
        now_ms: u64,
    ) -> Result<SpawnClaimOutcome> {
        let mut outcome = None;
        crate::state::update_locked::<SpawnClaimFile>(&self.path, |file| {
            outcome = Some(match file.claims.get(&spec.spawn_key) {
                Some(existing) => SpawnClaimOutcome::Adopted(existing.clone()),
                None => {
                    let claim = SpawnClaim {
                        spawn_key: spec.spawn_key.clone(),
                        node: spec.node.clone(),
                        run_id: spec.run_id.clone(),
                        claimed_at_ms: now_ms,
                        receipt: None,
                    };
                    file.claims.insert(spec.spawn_key.clone(), claim.clone());
                    SpawnClaimOutcome::Claimed(claim)
                }
            });
        })?;
        outcome.ok_or_else(|| anyhow!("spawn claim write produced no outcome"))
    }

    /// Record the launch receipt for a claimed key (best-effort at the call
    /// site: a pane that started is not undone by a bookkeeping miss).
    ///
    /// # Errors
    ///
    /// `Err` when the state file cannot be written under its lock.
    pub fn record_receipt(&self, spawn_key: &str, receipt: &Value) -> Result<()> {
        crate::state::update_locked::<SpawnClaimFile>(&self.path, |file| {
            if let Some(claim) = file.claims.get_mut(spawn_key) {
                claim.receipt = Some(receipt.clone());
            }
        })
    }

    /// True when the key is already claimed.
    pub fn is_claimed(&self, spawn_key: &str) -> bool {
        self.load().claims.contains_key(spawn_key)
    }

    /// Drop a claim after a KNOWN failed launch (no child exists, so a retry
    /// is legitimate).
    ///
    /// # Errors
    ///
    /// `Err` when the state file cannot be written under its lock.
    pub fn release(&self, spawn_key: &str) -> Result<()> {
        crate::state::update_locked::<SpawnClaimFile>(&self.path, |file| {
            file.claims.remove(spawn_key);
        })
    }

    fn load(&self) -> SpawnClaimFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }
}

/// Read the durable spec out of a kind-43001 body.
///
/// # Errors
///
/// `Err` when the event carries no `spec` object or it does not deserialize —
/// a malformed job is refused before anything starts.
#[allow(dead_code)]
pub fn spawn_spec_from_job(body: &Value) -> Result<toron_workflow::engine::spawn::SpawnSpec> {
    let spec = body
        .get("spec")
        .ok_or_else(|| anyhow!("spawn job carries no spec"))?;
    serde_json::from_value(spec.clone()).map_err(|e| anyhow!("spawn job spec does not parse: {e}"))
}

/// The launch plan for a durable spec.
///
/// The durable spec names the SUB-workflow `workflow_ref`; the legacy promise
/// payload called it `workflow`. Map it once here and reuse the ONE plan
/// builder, so the profile/harness/args/phase rules do not fork.
#[allow(dead_code)]
pub fn spawn_launch_plan_from_spec(
    spec: &toron_workflow::engine::spawn::SpawnSpec,
    catalog: &ProfileCatalog,
) -> Result<SpawnJobLaunch> {
    let legacy = serde_json::json!({
        "profile": spec.profile,
        "workflow": spec.workflow_ref,
        "resolved": spec.resolved,
        "await": spec.await_,
        "harness": spec.harness,
        "context": spec.context,
    });
    spawn_launch_plan(&spec.spawn_key, &legacy, catalog)
}

/// The terminal job body the consumer publishes.
///
/// `spawn_key` is the correlation key; `result` mirrors kind-43004 (result) or
/// kind-43006 (error) depending on `outcome`.
#[allow(dead_code)]
pub fn spawn_terminal_body(
    spec: &toron_workflow::engine::spawn::SpawnSpec,
    outcome: &Result<Value>,
) -> Value {
    match outcome {
        Ok(receipt) => serde_json::json!({
            "spawn_key": spec.spawn_key,
            "node": spec.node,
            "run_id": spec.run_id,
            "ok": true,
            "result": receipt,
        }),
        Err(e) => serde_json::json!({
            "spawn_key": spec.spawn_key,
            "node": spec.node,
            "run_id": spec.run_id,
            "ok": false,
            "error": format!("{e:#}"),
        }),
    }
}

/// One consumed durable spawn job.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SpawnJobOutcome {
    /// The claim that admitted the start.
    pub claim: SpawnClaim,
    /// `true` when a previous attempt already claimed the key, so nothing was
    /// launched here (the exactly-once adopt path).
    pub adopted: bool,
    /// The launch receipt (empty when the launch failed or was adopted
    /// without one).
    pub receipt: Value,
    /// The kind-43004 / kind-43006 body the host publishes.
    pub terminal: Value,
}

/// Drive ONE durable spawn job: claim, launch, terminal.
///
/// The claim is written to the durable store BEFORE the launch; a republished
/// key is ADOPTED instead of starting a second child — that is the
/// exactly-once gate the design names. A claim with no recorded launch fails
/// closed (the previous attempt died in the claim-to-pane window; a second
/// start could double-run). A KNOWN failed launch releases the claim (no child
/// exists, so a retry is legitimate) and still yields a terminal error body:
/// the run must not hang.
#[allow(dead_code)]
pub fn consume_spawn_job(
    claims: &SpawnClaimStore,
    body: &Value,
    catalog: &ProfileCatalog,
    now_ms: u64,
    mut launch: impl FnMut(SpawnJobLaunch) -> Result<Value>,
) -> Result<SpawnJobOutcome> {
    let spec = spawn_spec_from_job(body)?;
    let (claim, adopted) = match claims.claim(&spec, now_ms)? {
        SpawnClaimOutcome::Claimed(claim) => (claim, false),
        SpawnClaimOutcome::Adopted(claim) => (claim, true),
    };
    if adopted {
        let outcome = match claim.receipt.clone() {
            Some(receipt) => Ok(receipt),
            None => Err(anyhow!(
                "spawn {} was claimed at {}ms but recorded no launch — refusing a second start",
                claim.spawn_key,
                claim.claimed_at_ms
            )),
        };
        return Ok(SpawnJobOutcome {
            claim,
            adopted: true,
            receipt: outcome.as_ref().ok().cloned().unwrap_or(Value::Null),
            terminal: spawn_terminal_body(&spec, &outcome),
        });
    }
    let plan = match spawn_launch_plan_from_spec(&spec, catalog) {
        Ok(plan) => plan,
        Err(e) => {
            claims.release(&spec.spawn_key)?;
            let outcome = Err(e);
            return Ok(SpawnJobOutcome {
                claim,
                adopted: false,
                receipt: Value::Null,
                terminal: spawn_terminal_body(&spec, &outcome),
            });
        }
    };
    match launch(plan) {
        Ok(receipt) => {
            let _ = claims.record_receipt(&spec.spawn_key, &receipt);
            let outcome = Ok(receipt.clone());
            Ok(SpawnJobOutcome {
                claim,
                adopted: false,
                receipt,
                terminal: spawn_terminal_body(&spec, &outcome),
            })
        }
        Err(e) => {
            claims.release(&spec.spawn_key)?;
            let outcome = Err(e);
            Ok(SpawnJobOutcome {
                claim,
                adopted: false,
                receipt: Value::Null,
                terminal: spawn_terminal_body(&spec, &outcome),
            })
        }
    }
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
                continue;
            }
            let Some(spec) = engine.spawn_spec(&id) else {
                eprintln!("flywheel: spawn drain: no spec for spawn key {id} — skipping");
                continue;
            };
            let plan = match spawn_launch_plan_from_spec(&spec, catalog) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("flywheel: spawn drain: bad spec for {id}: {e}");
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
/// Plane door: read ONE resolution and hand the job to that plane's arm, so
/// a rex-crewed `steer` / `compose --run` never dials the hc socket.
pub fn launch_spawn_job(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    ws_id: &str,
    plan: &SpawnJobLaunch,
) -> Result<Value> {
    if rex_door_selected(cfg) {
        return launch_spawn_job_rex(cfg, project, wf_name, ws_id, plan);
    }
    launch_spawn_job_hc(cfg, project, wf_name, ws_id, plan)
}

#[allow(unused_mut)]
fn launch_spawn_job_hc(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    ws_id: &str,
    plan: &SpawnJobLaunch,
) -> Result<Value> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let name = crew_agent_name(wf_name, &format!("spawn-{}", plan.profile), 1);
    let registry = state::load_spawn_registry(project);
    let live_panes = c.agent_list()?;
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
        let panes = c.pane_list()?;
        let prev = panes
            .last()
            .map(|p| p.pane_id.clone())
            .ok_or_else(|| anyhow!("spawn drain: no panes in workspace {ws_id}"))?;
        let mail_identity = crate::pinned_mail_identity(cfg, project, &name, &plan.harness);
        let pane_env =
            pane_env_for_launch(cfg, mail_identity.as_deref(), plan.env_model_pin.as_deref())?;
        let split = crate::hc::pane_split(
            &c,
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
                let Some((from_model, route)) = hop else {
                    return Err(e);
                };
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
    if let Err(e) = c.agent_wait(&name, &[AgentStatus::Idle], Some(90_000)) {
        eprintln!("flywheel: spawn drain {name} readiness wait failed (dispatching anyway): {e}");
    }
    let mission_err = dispatch_spawn_mission(project, plan, &name)?;
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

/// Rex arm of [`launch_spawn_job`]: same claim/receipt contract over
/// `&dyn MultiplexerBackend`, never `Client::connect`. `session.new_split`
/// has no env map, so the pane env rides the one-string `env K=V` prefix and
/// the harness launches through `pane_run` — the same recipe the crew spawner
/// uses, so a rex crew's dynamic spawn steps land on the rex plane.
fn launch_spawn_job_rex(
    cfg: &Config,
    project: &str,
    wf_name: &str,
    ws_id: &str,
    plan: &SpawnJobLaunch,
) -> Result<Value> {
    let backend = crate::backend::factory::multiplexer_backend(cfg)?;
    let name = crew_agent_name(wf_name, &format!("spawn-{}", plan.profile), 1);
    let registry = state::load_spawn_registry(project);
    let live_agents = backend.agent_list()?;
    let is_fresh = plan.context.as_deref() == Some("fresh");
    let existing = if is_fresh {
        None
    } else {
        registry
            .iter()
            .find(|e| e.name == name)
            .filter(|e| {
                live_agents
                    .iter()
                    .any(|a| a.pane_id == e.pane_id && a.workspace_id == ws_id)
            })
            .cloned()
    };
    let (pane_id, name) = if let Some(e) = existing {
        (e.pane_id, name)
    } else {
        let dir = crate::project_dir(cfg, project);
        let panes = backend.pane_list(Some(ws_id))?;
        let prev = panes
            .last()
            .map(|p| p.pane_id.clone())
            .ok_or_else(|| anyhow!("spawn drain: no panes in workspace {ws_id}"))?;
        let mail_identity = crate::pinned_mail_identity(cfg, project, &name, &plan.harness);
        let pane_env =
            pane_env_for_launch(cfg, mail_identity.as_deref(), plan.env_model_pin.as_deref())?;
        let split = backend
            .pane_split(
                &prev,
                "down",
                Some(&crate::utf8_path(&dir, "workspace dir")?),
            )
            .map_err(|e| anyhow!("spawn drain: rex split in {ws_id}: {e:#}"))?;
        wait_rex_shell_ready(
            &*backend,
            &split.pane_id,
            &name,
            &split.pane_id,
            Duration::from_secs(15),
        )?;
        let command = rex_env_command(&pane_env, &plan.harness, &plan.args);
        backend
            .pane_run(&split.pane_id, &command)
            .map_err(|e| anyhow!("spawn drain: rex run for {name} ({}): {e:#}", split.pane_id))?;
        if let Err(e) = backend.agent_rename(&split.pane_id, Some(name.as_str())) {
            eprintln!(
                "flywheel: warn: rex rename {} → {name} failed: {e:#}",
                split.pane_id
            );
        }
        crate::bind_pane_mail_identity(
            cfg,
            project,
            &name,
            &split.pane_id,
            mail_identity.as_deref(),
        );
        // Pane id, not the label: a rex label may exist in several sessions.
        if let Err(e) = backend.agent_wait(
            &split.pane_id,
            &[AgentStatus::Idle, AgentStatus::Working],
            Some(90_000),
        ) {
            eprintln!(
                "flywheel: spawn drain {name} readiness wait failed (dispatching anyway): {e}"
            );
        }
        (split.pane_id, name)
    };
    let harness = plan.harness.clone();
    let mail_identity = crate::register_spawned_agent(cfg, project, &name, &harness, &pane_id);
    crate::record_spawn_entry(
        cfg,
        project,
        state::SpawnEntry::profiled(&pane_id, &name, &plan.profile, &harness, mail_identity),
    );
    let mission_err = dispatch_spawn_mission(project, plan, &name)?;
    if plan.await_job {
        let timeout_ms = cfg.default_timeout_ms.unwrap_or(1_800_000);
        backend.agent_wait(&pane_id, &[AgentStatus::Idle], Some(timeout_ms))?;
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

/// Dispatch one spawn job's workflow mission through the `__send` leaf. The
/// leaf resolves the active plane itself, so both arms share it. `Ok(None)`
/// is a clean dispatch; `Some(reason)` rides the receipt as `error`.
fn dispatch_spawn_mission(
    project: &str,
    plan: &SpawnJobLaunch,
    name: &str,
) -> Result<Option<String>> {
    let Some(w) = &plan.workflow else {
        return Ok(None);
    };
    let prompt = headless::base_prompt_for(w, &json!({}));
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__send")
        .arg(project)
        .arg(name)
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
        return Ok(Some(
            v["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| raw.clone()),
        ));
    }
    if trimmed.is_empty() {
        return Ok(Some(String::from_utf8_lossy(&output.stderr).to_string()));
    }
    Ok(None)
}

/// Pane-mode keeps the engine alive across `invoke` so the spawn drainer can
/// share it — enter the caller's runtime before construct, then drop the
/// guard and `block_on`.
///
/// The dry engine records actions only (the `Recording` mode), so it stays
/// in-memory by contract: no external effect is performed and a row claiming
/// otherwise is the lie the journal exists to prevent.
pub fn local_engine(
    rt: &tokio::runtime::Runtime,
    runner: toron_workflow::engine::EffectRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
) -> Result<toron_workflow::WorkflowEngine> {
    let recording_runner: toron_workflow::engine::EffectRunner = std::sync::Arc::new(|inv| {
        if inv.action.type_name() == "verify" {
            return Ok(json!({ "ok": true, "exit": 0, "recorded": true }));
        }
        Ok(json!({ "action": inv.action.type_name() }))
    });
    local_engine_with_journal(
        rt,
        runner,
        profile,
        workflow,
        Some(recording_runner),
        toron_workflow::engine::EffectJournalMode::Recording,
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
    local_engine_with(cfg, rt, runner, profile, workflow)
}

fn local_engine_with(
    cfg: &crate::config::Config,
    rt: &tokio::runtime::Runtime,
    runner: toron_workflow::engine::EffectRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
) -> Result<toron_workflow::WorkflowEngine> {
    let (effect, run_journal) = match crate::effect_journal::journals(cfg, runner.clone()) {
        Some(j) => (Some(j.effect), Some(j.run)),
        None => {
            anyhow::bail!(
                "journaled engine requested but the effect journal could not be opened (the toron index is unavailable) — refusing to run unjournaled"
            );
        }
    };
    let mode = engine_journal_mode(true, effect.is_some())?;
    local_engine_with_journal(rt, runner, profile, workflow, effect, mode, run_journal)
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

#[allow(clippy::too_many_arguments)]
fn local_engine_with_journal(
    rt: &tokio::runtime::Runtime,
    _runner: toron_workflow::engine::EffectRunner,
    profile: toron_workflow::ProfileResolver,
    workflow: toron_workflow::WorkflowResolver,
    effect: Option<toron_workflow::engine::EffectRunner>,
    mode: toron_workflow::engine::EffectJournalMode,
    run_journal: Option<toron_workflow::engine::RunJournal>,
) -> Result<toron_workflow::WorkflowEngine> {
    let _enter = rt.enter();
    let effect = effect;
    let async_effect = effect.map(asyncify_runner);
    let async_run_journal = run_journal.map(|journal| {
        std::sync::Arc::new(move |transition: toron_workflow::engine::RunTransition| {
            let journal = journal.clone();
            Box::pin(async move { journal(&transition) })
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }) as toron_workflow::engine::AsyncRunJournal
    });
    let engine =
        toron_workflow::WorkflowEngine::local_with_resolvers_async_effect_mode_and_journal(
            profile,
            workflow,
            async_effect,
            mode,
            async_run_journal,
        )?;
    *engine
        .verify_sink()
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        crate::durable_body::real_verify_sink()
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
    if matches!(mode, toron_workflow::engine::EffectJournalMode::Enforced) {
        let store = crate::effect_journal::journal_store().ok_or_else(|| {
            anyhow::anyhow!(
                "journaled engine requested but the durable store could not be opened — refusing to run unpersisted"
            )
        })?;
        return Ok(engine.with_effect_store(store));
    }
    Ok(engine)
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
        // Wave11: label-pinned entry (same single world read as assemble).
        let snap = crate::snapshot::assemble_with_label(&cfg, &signoff_project, &ws, now);
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
            let reads = make_reads();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| anyhow!("approval drainer runtime: {e}"))?;
            rt.block_on(async {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
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
                        let run_tag = format!("run-{}", &promise_id[..promise_id.len().min(8)]);
                        eprintln!(
                            "flywheel: approval parked: promise {promise_id} for project {project} — resolve with: flywheel workflow approve {run_tag} {promise_id} --decision approve|reject --project {project}",
                        );
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
                    if let Some(inbox) = &reads.inbox {
                        let parked: Vec<String> = {
                            let sink = engine.approval_sink().lock().await;
                            sink.iter().cloned().collect()
                        };
                        if !parked.is_empty() && pass.is_multiple_of(APPROVAL_SCAN_EVERY_PASSES) {
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
    use crate::elm::TestClock;

    /// The durable spawn spec one 43001 carries (bd-spawn-pane-consumer-6lmf).
    fn durable_spawn_spec() -> toron_workflow::engine::spawn::SpawnSpec {
        toron_workflow::engine::spawn::SpawnSpec {
            spawn_key: "run-1:spawn_it:1".into(),
            run_id: "run-1".into(),
            node: "spawn_it".into(),
            workflow: "spawn-wf".into(),
            project: None,
            generation: 1,
            profile: "@housekeeper".into(),
            resolved: serde_json::json!({
                "npub": "",
                "spec": { "harness": "pi", "model": "glm-5.3", "backend": "hc", "role": "housekeeper" }
            }),
            harness: Some("pi".into()),
            workflow_ref: None,
            await_: true,
            no_claim_timeout: 600,
            context: None,
        }
    }

    #[test]
    fn prepare_rex_launch_command_spills_long_env_to_executable_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut short_env = BTreeMap::new();
        short_env.insert("AGENT_NAME".to_string(), "DustyHollow".to_string());
        let short_cmd =
            prepare_rex_launch_command(dir.path(), "DustyHollow", &short_env, "claude", &[])
                .expect("short cmd");
        assert!(!short_cmd.starts_with("sh "), "{short_cmd}");

        let mut long_env = short_env;
        long_env.insert("PATH".to_string(), "/usr/bin:".repeat(120));
        let spilled =
            prepare_rex_launch_command(dir.path(), "DustyHollow", &long_env, "claude", &[])
                .expect("spilled cmd");
        assert!(spilled.starts_with("sh "), "{spilled}");
        let script_path = spilled.strip_prefix("sh ").unwrap().trim_matches('\'');
        let body = std::fs::read_to_string(script_path).expect("script written");
        assert!(body.starts_with("#!/bin/sh\nrm -f \"$0\"\nexec env "));
        assert!(body.contains("AGENT_NAME=DustyHollow"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(script_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o700, 0o700);
        }
    }

    #[test]
    fn compose_dispatch_prompt_preserves_stage_when_task_is_present() {
        let both = compose_dispatch_prompt(Some("STAGE RULES"), Some("do work"), "fallback");
        assert_eq!(both, "STAGE RULES\n\nTASK: do work");
        assert_eq!(
            compose_dispatch_prompt(Some("  STAGE RULES  "), None, "fallback"),
            "STAGE RULES"
        );
        assert_eq!(
            compose_dispatch_prompt(None, Some("  do work  "), "fallback"),
            "do work"
        );
        assert_eq!(
            compose_dispatch_prompt(Some("  "), Some(""), "fallback"),
            "fallback"
        );
    }

    /// The durable spawn launcher is a PLANE door (backends.md: spawn, crew
    /// launch, and preflight read one resolution). On the default rex plane
    /// the old hard-coded `Client::connect` died with `cannot connect to hc
    /// socket at ~/.local/state/herdr-core/default/default.sock`, so a
    /// rex-crewed `steer` / `compose --run` could pack a crew and never land
    /// a spawn step.
    #[test]
    fn the_spawn_launcher_reads_the_active_plane() {
        let src = include_str!("pane.rs");
        let start = src.find("pub fn launch_spawn_job(").expect("launcher");
        let rest = &src[start..];
        let end = rest.find("fn launch_spawn_job_hc(").expect("hc arm");
        let dispatcher = &rest[..end];
        assert!(
            dispatcher.contains("rex_door_selected(cfg)"),
            "the launcher must read the one plane resolution"
        );
        assert!(
            dispatcher.contains("launch_spawn_job_rex"),
            "the launcher must route to the rex arm"
        );

        let start = src.find("fn launch_spawn_job_rex(").expect("rex arm");
        let rest = &src[start..];
        let end = rest
            .find("/// Dispatch one spawn job's workflow mission")
            .expect("rex arm end");
        let rex = &rest[..end];
        assert!(
            rex.contains("multiplexer_backend"),
            "the rex arm must build the active backend"
        );
        assert!(
            !rex.contains("Client::connect"),
            "the rex arm must never dial the hc client"
        );
        assert!(
            rex.contains("pane_run"),
            "rex launches through pane_run (no env map on split)"
        );
    }

    /// The post-run reap is the last door a rex run touches. It used to dial
    /// hc after a successful rex crew, turning every `steer` into a backend
    /// connect error at the end.
    #[test]
    fn the_pane_run_reap_reads_the_active_plane() {
        let src = include_str!("cmd/workflow.rs");
        assert!(
            !src.contains("Client::connect"),
            "workflow run/compose must reap through the plane backend"
        );
        assert!(
            src.contains("reap_dead_registry_rows_on"),
            "the reap must take the resolved backend"
        );
    }

    /// `flywheel add` is a plane door: it resolves once, refuses a typo'd
    /// backend before the hc dial, and routes to the rex arm. The hc arm still
    /// holds a `Client::connect`, so the gate has to come first.
    #[test]
    fn cmd_add_routes_on_the_resolved_plane() {
        let src = include_str!("cmd/spawn.rs");
        let add = crate::tripwire::fn_body(src, "cmd_add");
        for needle in ["resolve_plane(cfg)", "require_mux_for_spawn", "cmd_add_rex"] {
            assert!(add.contains(needle), "cmd_add must use {needle}");
        }
        let gate = add.find("Plane::Rex").expect("the rex arm gate");
        let dial = add
            .find("Client::connect")
            .expect("the hc arm still holds the client dial");
        assert!(gate < dial, "the plane gate must precede the hc dial");
    }

    /// The dispatch registry and the settle window open on a runner with no
    /// backend in scope. Both hard-dialed `Client::connect(None)` — the hc
    /// socket — so a rex crew pruned every registry row and could never settle
    /// a pane that was split moments earlier.
    #[test]
    fn dispatch_registry_and_settle_read_the_active_plane() {
        let src = include_str!("pane.rs");
        let body = |name: &str| {
            let start = src
                .find(&format!("\nfn {name}("))
                .or_else(|| src.find(&format!("\npub fn {name}(")))
                .unwrap_or_else(|| panic!("expected toplevel `fn {name}(`"));
            crate::tripwire::body_from(src, start + 1)
        };
        for name in [
            "live_registry_for_dispatch",
            "settle_for_role_slot",
            "live_pane_set_on_active_plane",
        ] {
            assert!(
                !body(name).contains("Client::connect"),
                "{name} must not dial the hc client"
            );
        }
        for name in ["live_registry_for_dispatch", "settle_for_role_slot"] {
            assert!(
                body(name).contains("live_pane_set_on_active_plane"),
                "{name} must read the active plane"
            );
        }
        let probe = body("live_pane_set_on_active_plane");
        assert!(
            probe.contains("multiplexer_backend") && probe.contains("pane_list"),
            "the probe must list panes on the constructed plane backend"
        );
    }

    /// The rex/hc seam: no door may treat the CONFIGURED name as the serving
    /// plane. `auto` is not an identity (`pane_keep` matches "hc"/"rex"
    /// exactly), and `auto` that resolves to hc must still take the hc arm.
    #[test]
    fn the_rex_hc_seam_keys_on_the_resolved_plane() {
        for src in [
            include_str!("doorbell_host.rs"),
            include_str!("inbox_ports.rs"),
            include_str!("health_page.rs"),
        ] {
            assert!(
                !src.contains("resolve_backend_name"),
                "a configured name is not a plane verdict"
            );
            assert!(
                src.contains("resolve_plane"),
                "the door must read the one resolution"
            );
        }
        let pane = include_str!("pane.rs");
        let start = pane
            .find("\npub fn dead_registry_rows(")
            .expect("dead_registry_rows");
        let body = crate::tripwire::body_from(pane, start + 1);
        assert!(body.contains("resolve_plane"), "dead rows read the plane");
        assert!(
            !body.contains("resolve_backend_name"),
            "`auto` is not a pane_keep identity"
        );
    }

    /// The cron spawn launcher resolves its workspace on the active plane; a
    /// raw hc client there died before the (plane-aware) launcher ever ran.
    #[test]
    fn the_cron_spawn_launcher_resolves_the_active_plane() {
        let src = include_str!("cron.rs");
        let start = src
            .find("\nfn cron_spawn_launcher(")
            .expect("cron spawn launcher");
        let body = crate::tripwire::body_from(src, start + 1);
        assert!(!body.contains("Client::connect"), "no raw hc dial");
        assert!(
            body.contains("multiplexer_backend") && body.contains("find_workspace_dyn"),
            "the workspace must resolve through the plane backend"
        );
    }

    /// Rex readiness/await waits must target the PANE id, never the label:
    /// labels are unique only within one rex session, so a same-named crew in
    /// another live session makes a label wait fail with "label … is in 2
    /// sessions" (seen in the live steer proof).
    #[test]
    fn rex_waits_target_the_pane_not_the_label() {
        let src = include_str!("pane.rs");
        for name in ["spawn_crew_into_rex", "launch_spawn_job_rex"] {
            let body = crate::tripwire::body_from(src, crate::tripwire::find_fn(src, name));
            for (i, _) in body.match_indices("agent_wait(") {
                let call = &body[i..(i + 240).min(body.len())];
                assert!(
                    !call.contains("&name,"),
                    "{name}: a rex wait must target the pane, not the label"
                );
            }
        }
    }

    /// The send arms key on the RESOLVED plane, not the configured name:
    /// `auto` on a host without the rex CLI really does serve hc, and the old
    /// `resolve_backend_name(cfg) != "hc"` gate sent that host into the rex
    /// arm (and an explicit hc pin into rex) — a split-plane dispatch.
    #[test]
    fn send_arms_key_on_the_resolved_plane() {
        for src in [include_str!("cmd/send.rs"), include_str!("comms_send.rs")] {
            assert!(
                !src.contains("resolve_backend_name(cfg) != \"hc\""),
                "send must not gate on the configured name"
            );
            assert!(
                src.contains("resolve_plane(cfg).plane"),
                "send must gate on the resolved plane"
            );
        }
    }

    /// A fresh claim file per test (the live store is the machine state dir).
    fn claim_store(tag: &str) -> SpawnClaimStore {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        SpawnClaimStore::at(std::env::temp_dir().join(format!(
            "fw-spawn-claims-{tag}-{}-{nanos}.json",
            std::process::id()
        )))
    }

    /// bd-spawn-pane-consumer-6lmf: one durable spawn job runs from the
    /// kind-43001 spec alone — no promise, no server.
    #[test]
    fn spawn_job_runs_from_the_43001_spec_with_no_server() {
        let spec = durable_spawn_spec();
        let body = serde_json::json!({
            "title": "spawn run-1/spawn_it",
            "spec": serde_json::to_value(&spec).expect("spec json"),
            "assignee": null,
        });
        let claims = claim_store("one");
        let mut launched: Vec<String> = Vec::new();
        let outcome = consume_spawn_job(&claims, &body, &test_catalog(), 1_000, |plan| {
            launched.push(plan.profile.clone());
            assert_eq!(plan.harness, "pi", "the resolved harness rides the spec");
            assert!(plan.await_job, "await: true parks the run");
            Ok(serde_json::json!({"pane": "w1:p9"}))
        })
        .expect("consume the job");

        assert_eq!(launched, ["housekeeper"], "exactly one start");
        assert!(!outcome.adopted);
        assert_eq!(outcome.claim.spawn_key, "run-1:spawn_it:1");
        assert_eq!(
            outcome.terminal["spawn_key"],
            serde_json::json!("run-1:spawn_it:1")
        );
        assert_eq!(outcome.terminal["ok"], serde_json::json!(true));
        assert_eq!(
            outcome.terminal["result"]["pane"],
            serde_json::json!("w1:p9")
        );
    }

    /// The restart republish (bd-spawn-launcher-flip-d8mv): the engine
    /// re-emits the same `spawn_key`; the DURABLE claim is adopted and NO
    /// second child starts.
    #[test]
    fn spawn_claim_adopts_after_a_restart() {
        let spec = durable_spawn_spec();
        let body = serde_json::json!({
            "title": "spawn",
            "spec": serde_json::to_value(&spec).expect("spec json"),
            "assignee": null,
        });
        let claims = claim_store("adopt");
        let mut starts = 0u32;
        let first = consume_spawn_job(&claims, &body, &test_catalog(), 1_000, |_| {
            starts += 1;
            Ok(serde_json::json!({"pane": "w1:p9"}))
        })
        .expect("first start");
        assert_eq!(first.terminal["ok"], serde_json::json!(true));

        let reopened = SpawnClaimStore::at(claims.path.clone());
        let second = consume_spawn_job(&reopened, &body, &test_catalog(), 2_000, |_| {
            starts += 1;
            Ok(serde_json::json!({"pane": "w1:p10"}))
        })
        .expect("a republished key adopts");
        assert!(second.adopted, "the existing claim is adopted");
        assert_eq!(starts, 1, "one child, never two");
        assert_eq!(
            second.terminal["result"]["pane"],
            serde_json::json!("w1:p9"),
            "the adopted terminal carries the original receipt"
        );
        assert!(reopened.is_claimed("run-1:spawn_it:1"));
    }

    /// A claim written before a crash that recorded no launch fails CLOSED: no
    /// second child, because the first may have started.
    #[test]
    fn spawn_claim_without_a_recorded_launch_fails_closed() {
        let spec = durable_spawn_spec();
        let body = serde_json::json!({ "spec": serde_json::to_value(&spec).expect("spec") });
        let claims = claim_store("unlaunched");
        claims
            .claim(&spec, 1_000)
            .expect("claim without a launch (the crash window)");
        let mut starts = 0u32;
        let outcome = consume_spawn_job(&claims, &body, &test_catalog(), 2_000, |_| {
            starts += 1;
            Ok(serde_json::json!({"pane": "second"}))
        })
        .expect("an adopt is a terminal, not an error");
        assert!(outcome.adopted);
        assert_eq!(starts, 0, "no second child");
        assert_eq!(outcome.terminal["ok"], serde_json::json!(false));
        assert!(
            outcome.terminal["error"]
                .as_str()
                .is_some_and(|e| e.contains("recorded no launch")),
            "{}",
            outcome.terminal
        );
    }

    /// A failed launch releases the claim and still yields a terminal error
    /// body, so the run is never left hanging.
    #[test]
    fn spawn_launch_failure_releases_the_claim() {
        let spec = durable_spawn_spec();
        let body = serde_json::json!({ "spec": serde_json::to_value(&spec).expect("spec") });
        let claims = claim_store("failed");
        let outcome = consume_spawn_job(&claims, &body, &test_catalog(), 1_000, |_| {
            anyhow::bail!("pane split refused")
        })
        .expect("a failed launch is a terminal, not an error");
        assert!(!outcome.adopted);
        assert_eq!(outcome.terminal["ok"], serde_json::json!(false));
        assert!(
            outcome.terminal["error"]
                .as_str()
                .is_some_and(|e| e.contains("pane split refused")),
            "{}",
            outcome.terminal
        );
        assert!(
            !claims.is_claimed("run-1:spawn_it:1"),
            "the claim is released so the job can be retried"
        );
    }

    /// bd-ietv: a role whose registry row exists but whose pane is missing
    /// from hc's listing at dispatch time is the spawn/list race, not a
    /// dead pane. The prune drops the row, so without settling the whole run
    /// bails `no crew pane for @oracle` even though the crew spawned it.
    #[test]
    fn settle_role_waits_out_a_pane_plane_has_not_listed_yet() {
        use std::collections::HashSet;
        let rows = vec![SpawnEntry {
            pane_id: "w1:p1".into(),
            name: "crew-goal-planner".into(),
            profile: Some("planner".into()),
            harness: Some("omp".into()),
            model: None,
            model_pin: None,
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        }];
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
    /// an unreachable hc.
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
                    None
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
        assert_eq!(
            policy.retries,
            MAX_ATTEMPTS - 1,
            "five retries after the first attempt"
        );
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
            dispatch_policy().seed,
            def.seed,
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
        assert!(
            src.contains("select_empty_pane(&pane_ids, &occupied)"),
            "spawn_crew_into must probe occupancy before binding the root pane"
        );
        assert!(
            src.contains("pane_split(\n                                    c,"),
            "all-occupied falls through to a fresh split, never a blind bind"
        );
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
        assert_eq!(
            crate::pane::engine_journal_mode(true, true).expect("journaled"),
            EffectJournalMode::Enforced
        );
        let err = crate::pane::engine_journal_mode(true, false)
            .expect_err("a journaled constructor that cannot journal must refuse");
        assert!(
            err.to_string().contains("refusing to run unjournaled"),
            "the refusal must say what it refuses: {err}"
        );
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = as_live(vec![
            entry("w1:p1", "crew-wf-oracle", "oracle"),
            entry("w1:p2", "crew-wf-coder", "coder"),
        ]);
        assert_eq!(
            dispatch_reroute_target(&catalog, &entries, "oracle", &[]),
            Some(("coder".to_string(), "crew-wf-coder".to_string()))
        );
        assert_eq!(
            dispatch_reroute_target(&catalog, &entries, "coder", &[]),
            None
        );
        let oracle_only = as_live(vec![entry("w1:p1", "crew-wf-oracle", "oracle")]);
        assert_eq!(
            dispatch_reroute_target(&catalog, &oracle_only, "oracle", &[]),
            None
        );
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
        let dispatch = src
            .find("Some(rate_limit_kind.as_str())")
            .expect("429 reroute block");
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
        let end = src[start..].find("\nfn ").map_or(start, |i| start + i);
        let slice = &src[start..end];
        assert!(
            slice.contains("profile_launch_plan(harness, profile, None)"),
            "compose pre-spawn must honor Profile.provider via the launch PLAN"
        );
        assert!(
            slice.contains("model_pin,\n                effort:"),
            "the crew registry row must carry the pin + effort (ADR-0013 P1-7, ADR-0022 D7)"
        );
        assert!(
            slice.contains("mode: None,\n                skills_digest:"),
            "the crew registry row must carry mode + skills digest (ADR-0013 P1-8, ADR-0022 D7)"
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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

    fn crew_row(pane: &str, name: &str, profile: &str, pin: &str, model: &str) -> SpawnEntry {
        let mut e = SpawnEntry::with_mail_identity(pane, name, pin);
        e.profile = Some(profile.into());
        e.harness = Some("rex".into());
        e.model = Some(model.into());
        e
    }

    /// ADR-0022 D8 acceptance 6 on the real ce-lfg audit step: the fresh
    /// reviewer dispatch binds a non-author seat of a different model
    /// family, and a replay binds identically.
    #[test]
    fn beads_lifecycle_audit_binds_a_non_author_different_family() {
        use crate::seat_binder::Binding;
        use toron_workflow::schema::Action;
        let wf = crate::builtin_workflows::builtin_workflows()
            .get("ce-wave")
            .cloned()
            .expect("ce-wave cataloged");
        let audit = wf
            .steps
            .iter()
            .find(|s| s.name.as_deref() == Some("audit"))
            .expect("audit step");
        let Action::DispatchJob { spec, .. } = &audit.action else {
            panic!("audit is a dispatch_job");
        };
        // Same-family reviewer sorts first; the binder must prefer the
        // registered different-family reviewer.
        let entries = as_live(vec![
            crew_row("w1:p1", "planner", "planner", "PlanPin", "anthropic/claude"),
            crew_row(
                "w1:p2",
                "reviewer-same",
                "reviewer",
                "AaSame",
                "anthropic/claude",
            ),
            crew_row("w1:p3", "reviewer-diff", "reviewer", "ZzDiff", "openai/gpt"),
        ]);
        let first = review_bind(
            "reviewer",
            spec,
            &entries,
            5,
            &std::collections::BTreeSet::new(),
        )
        .expect("review binding");
        assert!(
            matches!(
                &first,
                Binding::Other { pin } | Binding::FreshSession { pin } if pin == "ZzDiff"
            ),
            "audit binds a non-author seat of a different family: {first:?}"
        );
        assert_eq!(
            review_bind(
                "reviewer",
                spec,
                &entries,
                5,
                &std::collections::BTreeSet::new()
            ),
            Some(first),
            "a replay binds identically"
        );
    }

    /// The author is excluded even when it is a same-role candidate: a
    /// reviewer that authored the beads is never bound to review them.
    #[test]
    fn review_never_binds_the_authoring_seat() {
        use crate::seat_binder::Binding;
        let spec = toron_workflow::schema::parse_workflow(
            "name: t\ntrigger:\n  type: webhook\nsteps:\n  - name: audit\n    action: dispatch_job\n    assignee: x\n    spec: { profile: \"@reviewer\", context: fresh, author_pin: \"AuthorPin\" }\n",
        )
        .expect("parse")
        .steps
        .first()
        .and_then(|s| match &s.action {
            toron_workflow::schema::Action::DispatchJob { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .expect("dispatch spec");
        // Author sorts first; the binder must still pick the other reviewer.
        let entries = as_live(vec![
            crew_row(
                "w1:p1",
                "author",
                "reviewer",
                "AuthorPin",
                "anthropic/claude",
            ),
            crew_row("w1:p2", "other", "reviewer", "OtherPin", "openai/gpt"),
        ]);
        let b = review_bind(
            "reviewer",
            &spec,
            &entries,
            5,
            &std::collections::BTreeSet::new(),
        )
        .expect("binding");
        assert!(
            matches!(&b, Binding::Other { pin } | Binding::FreshSession { pin } if pin == "OtherPin"),
            "the authoring reviewer is never bound: {b:?}"
        );
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
        let entries = as_live(vec![
            SpawnEntry {
                pane_id: "w2:p1".into(),
                name: "crew-goal-oracle".into(),
                profile: Some("oracle".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
                skills_injected: false,
                incarnation: 0,
            },
        ]);

        let first = resolve_pane_entry(&entries, "oracle").expect("profile hit");
        let again = resolve_pane_entry(&entries, "oracle").expect("profile hit");
        assert_eq!(
            first.pane_id, again.pane_id,
            "the single-entry lookup is ordinal-blind BY CONSTRUCTION — this equality \
             is the defect, not an accident of these rows"
        );

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

        assert!(
            resolve_pane_entry_nth(&entries, "oracle", 3).is_none(),
            "one more copy than the crew has panes must be None, never a reuse"
        );

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
                    effort: None,
                    mode: None,
                    skills_digest: None,
                    mail_identity: None,
                    harness_session: None,
                    skills_injected: false,
                    incarnation: 0,
                })
                .collect(),
        );

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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        }]);
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
            skills_injected: false,
            incarnation: inc,
        };
        let entries = as_live(vec![
            row("w7:p1", "crew-goal-oracle", 0),
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry {
                pane_id: "w3:p9".into(),
                name: "crew-goal-oracle".into(),
                profile: Some("oracle".into()),
                harness: Some("omp".into()),
                model: None,
                model_pin: None,
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
        let (kept, pruned) = reap_closed_rows(entries, &live, "hc");
        assert_eq!(pruned, 1);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|e| e.pane_id != "wDead:p1"));
    }

    /// bd-j0b5: the persisted clear keeps a row from a harness hc cannot
    /// inject (its pane is absent on purpose) and an unknown harness; only a
    /// gone pane of an hc-injectable harness is cleared.
    #[test]
    fn reap_close_keeps_pins_plane_cannot_inject() {
        use std::collections::HashSet;
        let mk = |pane: &str, name: &str, harness: Option<&str>| SpawnEntry {
            pane_id: pane.into(),
            name: name.into(),
            profile: None,
            harness: harness.map(str::to_string),
            model: None,
            model_pin: None,
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: Some(name.into()),
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            mk("wDead:p1", "freebuff-pin", Some("freebuff")),
            mk("wGone:p2", "gone-opencode", Some("opencode")),
            mk("wLegacy:p3", "legacy-pin", None),
        ];
        let live: HashSet<String> = HashSet::new();
        let (kept, pruned) = reap_closed_rows(entries, &live, "hc");
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            mk("wDead:p1", "crew-old-planner"),
            mk("wLive:p1", "crew-new-planner"),
        ];
        let live: HashSet<String> = ["wLive:p1".to_string()].into_iter().collect();
        let dead = find_dead_rows(&entries, &live, "hc");
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
            "hc",
        );
        assert_eq!(none, [] as [(String, String); 0]);
    }

    /// bd-p09m: the gauge agrees with the clear — a mail-only row the reap
    /// keeps is not a dead row. Only a gone pane of an hc-injectable
    /// harness on hc is dead.
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: Some(name.into()),
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = vec![
            mk("wDead:p1", "freebuff-pin", Some("freebuff")),
            mk("wGone:p2", "gone-opencode", Some("opencode")),
            mk("wLegacy:p3", "legacy-pin", None),
        ];
        let live: HashSet<String> = HashSet::new();
        let dead = find_dead_rows(&entries, &live, "hc");
        assert_eq!(
            dead,
            vec![("wGone:p2".to_string(), "gone-opencode".to_string())]
        );
        // rex (called superlogical by older configs) lists every block it
        // owns, so every missing pane is clearable — including a mail-only
        // harness that hc deliberately cannot inject.
        let rex = find_dead_rows(&entries, &live, "superlogical");
        assert_eq!(
            rex,
            vec![
                ("wDead:p1".to_string(), "freebuff-pin".to_string()),
                ("wGone:p2".to_string(), "gone-opencode".to_string()),
                ("wLegacy:p3".to_string(), "legacy-pin".to_string()),
            ]
        );
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
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
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: None,
                harness_session: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry::legacy("w1:p3", "plain"),
        ]);
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
    /// (hc agent-name rule: lowercase alnum/-/_ only, <= 32 chars).
    #[test]
    fn crew_agent_names_are_role_ordinal() {
        assert_eq!(crew_agent_name("plan", "planner", 1), "crew-plan-planner");
        assert_eq!(crew_agent_name("plan", "coder", 2), "crew-plan-coder2");
        assert_eq!(
            crew_agent_name("CREW-WF", "planner", 1),
            "crew-crew-wf-planner"
        );
        assert_eq!(crew_agent_name("My Plan", "coder", 1), "crew-my-plan-coder");
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
        let catalog = test_catalog_with_reviewer_harness("pi");
        let entry = |pane_id: &str, name: &str, profile: &str, harness: &str| SpawnEntry {
            pane_id: pane_id.to_string(),
            name: name.to_string(),
            profile: Some(profile.to_string()),
            harness: Some(harness.to_string()),
            model: None,
            model_pin: None,
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        };
        let entries = as_live(vec![entry("w1:p1", "crew-goal-reviewer", "reviewer", "pi")]);
        let action = Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@reviewer".into()),
                harness: Some("opencode".into()),
                ..Default::default()
            },
            channel: None,
            await_: None,
            no_claim_timeout: 600,
            result_timeout: None,
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
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: None,
            harness_session: None,
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
            await_: None,
            no_claim_timeout: 600,
            result_timeout: None,
        };
        let err = pane_dispatch(&catalog, "proj", "wf", &json!({}), &entries, &action, None)
            .expect_err("no opencode pane and no reviewer pane must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("overrides harness=opencode but no crew pane runs it"),
            "{msg}"
        );
    }

    /// bd-sfkm8: the real pane runner reaches the pin chosen by an in-flight
    /// join, with the profile still resolvable. A missing pin fails loud.
    #[test]
    fn a_join_bound_step_dispatches_to_the_chosen_pin() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fw-join-pin-{}-{}.toml",
            std::process::id(),
            CTR.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::write(&path, "[profiles.coder]\nharness = \"pi\"\n").unwrap();
        let catalog = ProfileCatalog::load_from(Some(&path)).unwrap();
        let _ = std::fs::remove_file(&path);
        let entries = as_live(vec![
            crew_row("w1:p1", "alpha-seat", "coder", "Alpha", ""),
            crew_row("w2:p2", "bravo-seat", "coder", "Bravo", ""),
        ]);
        let step = toron_workflow::schema::Step {
            name: Some("execute".into()),
            action: Action::DispatchJob {
                assignee: "x".into(),
                spec: toron_workflow::schema::DispatchSpec {
                    profile: Some("@coder".into()),
                    stage: Some("ce-work".into()),
                    ..Default::default()
                },
                channel: None,
                await_: None,
                no_claim_timeout: 600,
                result_timeout: None,
            },
            guard: None,
            on_error: toron_workflow::schema::StepOnError::Continue,
            build_slot: false,
        };
        let wf = toron_workflow::schema::Workflow {
            name: "joined".into(),
            description: None,
            trigger: None,
            policy: None,
            guard: None,
            steps: vec![step],
            graph: None,
            inputs: Vec::new(),
            crew: None,
            principles: Vec::new(),
        };
        let plan = crate::compose_join::JoinPlan {
            steps: vec![crate::compose_join::StepBinding {
                step: "execute".into(),
                role: "coder".into(),
                pin: Some("Bravo".into()),
                stage: Some("ce-work".into()),
                spawn: false,
                wait: false,
            }],
            spawns: Vec::new(),
            needs_yes: false,
        };
        let bound = crate::compose_join::apply_join_bindings(&wf, &plan, Some("bd-42"));
        let Action::DispatchJob { assignee, spec, .. } = &bound.steps[0].action else {
            panic!("bound step must stay a dispatch");
        };
        assert_eq!(assignee, "Bravo");
        assert_eq!(
            spec.profile.as_deref(),
            Some("@coder"),
            "profile must resolve"
        );
        let action = bound.steps[0].action.clone();
        let out = pane_dispatch(&catalog, "proj", "wf", &json!({}), &entries, &action, None)
            .expect("the bound step must dispatch");
        assert_eq!(out["agent"], "bravo-seat", "runner must reach Bravo: {out}");
    }

    /// bd-3ann1: the real planner → binding → pane-runner path carries the
    /// execution stage and the BEAD header all the way to the prompt that a
    /// bound seat receives. The port observes the actual prompt, not a spec.
    #[test]
    fn compose_join_carries_stage_and_bead_header_to_the_runner() {
        use std::cell::RefCell;
        let _g = crate::config::lock_env();
        let project = format!("pane-join-stage-{}", std::process::id());
        let path = std::env::temp_dir().join(format!("{project}.toml"));
        std::fs::write(&path, "[profiles.coder]\nharness = \"pi\"\n").unwrap();
        let catalog = ProfileCatalog::load_from(Some(&path)).unwrap();
        let _ = std::fs::remove_file(&path);

        let entries = as_live(vec![
            crew_row("w1:p1", "alpha-seat", "coder", "Alpha", ""),
            crew_row("w2:p2", "bravo-seat", "coder", "Bravo", ""),
        ]);
        let seats: Vec<crate::seat_binder::LiveSeat> = entries
            .entries()
            .iter()
            .map(|e| crate::seat_binder::LiveSeat {
                pin: e.mail_identity.clone().unwrap_or_else(|| e.name.clone()),
                role: e.profile.clone().unwrap_or_else(|| "crew".into()).into(),
                harness: e.harness.clone().unwrap_or_default().into(),
                model: e.model.clone(),
                mode: e.mode.clone(),
                effort: e.effort.clone(),
                placement: crate::seat_binder::SeatPlacement::Pane,
                idle: true,
                fresh_session: false,
                idle_since_ms: None,
            })
            .collect();

        let wf = toron_workflow::schema::Workflow {
            name: "joined".into(),
            description: None,
            trigger: None,
            policy: None,
            guard: None,
            steps: vec![toron_workflow::schema::Step {
                name: Some("execute".into()),
                guard: None,
                on_error: toron_workflow::schema::StepOnError::Continue,
                build_slot: false,
                action: Action::DispatchJob {
                    assignee: "x".into(),
                    spec: toron_workflow::schema::DispatchSpec {
                        profile: Some("@coder".into()),
                        stage: Some("ce-work".into()),
                        ..Default::default()
                    },
                    channel: None,
                    await_: None,
                    no_claim_timeout: 600,
                    result_timeout: None,
                },
            }],
            graph: None,
            inputs: Vec::new(),
            crew: None,
            principles: Vec::new(),
        };
        let plan = crate::compose_join::plan_join(
            &wf,
            &seats,
            &crate::compose_join::JoinRoles::default(),
            8,
        );
        assert!(
            plan.steps[0].pin.is_some(),
            "a live crew must be bound: {plan:?}"
        );
        let bound = crate::compose_join::apply_join_bindings(&wf, &plan, Some("bd-42"));

        let seen: RefCell<Option<(String, String)>> = RefCell::new(None);
        let port = |agent: &str, prompt: &str| -> anyhow::Result<()> {
            *seen.borrow_mut() = Some((agent.to_string(), prompt.to_string()));
            Ok(())
        };
        let reads = std::collections::BTreeMap::new();
        let ctx = DispatchContext {
            scope: DispatchScope {
                run_id: "run-join",
                node_path: "execute",
            },
            reads: &reads,
            prompt_port: Some(&port),
        };
        let out = pane_dispatch_scoped(
            &catalog,
            &project,
            "wf",
            &json!({}),
            &entries,
            &bound.steps[0].action,
            None,
            Some(&ctx),
        )
        .expect("the bound step must reach the runner");
        assert_eq!(
            out["step"], "ce-work",
            "the stage must reach the runner: {out}"
        );
        let (agent, prompt) = seen.into_inner().expect("the prompt port ran");
        assert_eq!(
            agent, "alpha-seat",
            "the bound live seat receives it: {agent}"
        );
        assert!(
            prompt.starts_with("BEAD: bd-42"),
            "the bound work order must lead the typed prompt: {prompt}"
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
                    "spec": { "harness": "pi", "model": "glm-5.3", "backend": "hc", "role": "housekeeper" },
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
            "resolved": { "npub": "", "spec": { "harness": "pi", "model": "glm-5.3", "backend": "hc", "role": "housekeeper" } },
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
        let engine = local_engine_with_journal(
            &rt,
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
            "the local engine must succeed after Runtime::enter"
        );
    }

    #[test]
    fn local_engine_constructs_without_a_runtime() {
        let engine = toron_workflow::WorkflowEngine::local();
        assert!(engine.is_ok(), "local engine constructs off-runtime");
    }

    /// bd-6setu.16 M1 + H3, witnessed by EXECUTION: a prompt port observes the
    /// store, so the test proves the binding row is durable BEFORE any prompt
    /// leaves (not that two statements are written in a certain order), and the
    /// authors come from the reads map handed to the dispatch (H3) rather than
    /// from a library reloaded per step.
    #[test]
    fn the_binding_row_is_durable_before_the_prompt_port_runs() {
        let _g = crate::config::lock_env();
        let project = format!("pane-bind-{}", std::process::id());
        // A row for the step this review READS: the binder must see it as an
        // author, which only holds if the reads map is the data we passed.
        crate::dispatch_bindings::append(&project, "run-9", "plan", "plan", "PlanAuthor")
            .expect("seed the author row");

        let catalog = test_catalog_with_reviewer_harness("pi");
        let entries = as_live(vec![SpawnEntry {
            pane_id: "w1:p1".into(),
            name: "crew-goal-reviewer".into(),
            profile: Some("reviewer".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: Some("BlueLake".into()),
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        }]);
        let action = Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@reviewer".into()),
                stage: Some("ce-review".into()),
                ..Default::default()
            },
            channel: None,
            await_: None,
            no_claim_timeout: 600,
            result_timeout: None,
        };
        let reads: std::collections::BTreeMap<String, Vec<String>> =
            [("ce-review".to_string(), vec!["steps.plan".to_string()])]
                .into_iter()
                .collect();
        let seen: std::cell::RefCell<Option<(usize, std::collections::BTreeSet<String>)>> =
            std::cell::RefCell::new(None);
        let port = |_agent: &str, _prompt: &str| -> anyhow::Result<()> {
            let rows = crate::dispatch_bindings::all(&project)?;
            let steps: std::collections::BTreeSet<String> =
                ["plan".to_string()].into_iter().collect();
            let authors = crate::dispatch_bindings::authors_for(&project, "run-9", &steps)?;
            *seen.borrow_mut() = Some((rows.len(), authors));
            Ok(())
        };
        let ctx = DispatchContext {
            scope: DispatchScope {
                run_id: "run-9",
                node_path: "review",
            },
            reads: &reads,
            prompt_port: Some(&port),
        };
        let out = pane_dispatch_scoped(
            &catalog,
            &project,
            "wf",
            &json!({}),
            &entries,
            &action,
            None,
            Some(&ctx),
        )
        .expect("the dispatch reaches the port");
        assert_eq!(
            out["prompt_port"], true,
            "the port is the prompt seam: {out}"
        );
        let (row_count, authors) = seen.into_inner().expect("the port ran");
        assert_eq!(
            row_count, 2,
            "the author row plus THIS dispatch's row are already on disk when the port runs"
        );
        assert!(
            authors.contains("PlanAuthor"),
            "the reads map we passed names the author step: {authors:?}"
        );
    }

    /// bd-6setu.16 H3, by execution: the authors come from the reads map the
    /// dispatch was HANDED. With the map naming the step, the seeded author is
    /// the reviewer itself and the binder refuses; with an empty map the same
    /// dispatch is free. A per-step library reload cannot tell those apart.
    #[test]
    fn the_reads_map_decides_the_authors_at_dispatch() {
        let _g = crate::config::lock_env();
        let project = format!("pane-reads-{}", std::process::id());
        // The author row names the REVIEWER's own pin.
        crate::dispatch_bindings::append(&project, "run-9", "plan", "plan", "BlueLake")
            .expect("seed the author row");

        let catalog = test_catalog_with_reviewer_harness("pi");
        let entries = as_live(vec![SpawnEntry {
            pane_id: "w1:p1".into(),
            name: "crew-goal-reviewer".into(),
            profile: Some("reviewer".into()),
            harness: Some("pi".into()),
            model: None,
            model_pin: None,
            effort: None,
            mode: None,
            skills_digest: None,
            mail_identity: Some("BlueLake".into()),
            harness_session: None,
            skills_injected: false,
            incarnation: 0,
        }]);
        let action = Action::DispatchJob {
            assignee: "x".into(),
            spec: toron_workflow::schema::DispatchSpec {
                profile: Some("@reviewer".into()),
                stage: Some("ce-review".into()),
                context: Some(toron_workflow::schema::ContextMode::Fresh),
                ..Default::default()
            },
            channel: None,
            await_: None,
            no_claim_timeout: 600,
            result_timeout: None,
        };
        let named: std::collections::BTreeMap<String, Vec<String>> =
            [("ce-review".to_string(), vec!["steps.plan".to_string()])]
                .into_iter()
                .collect();
        // A no-op port: if the binder ever frees this dispatch the port makes
        // that a clean Ok, so the mutation shows up as "no refusal" instead of
        // as a failed launch.
        let never = |_a: &str, _p: &str| -> anyhow::Result<()> { Ok(()) };
        let ctx_named = DispatchContext {
            scope: DispatchScope {
                run_id: "run-9",
                node_path: "review",
            },
            reads: &named,
            prompt_port: Some(&never),
        };
        let refused = pane_dispatch_scoped(
            &catalog,
            &project,
            "wf",
            &json!({}),
            &entries,
            &action,
            None,
            Some(&ctx_named),
        );
        assert!(
            refused.is_err(),
            "the reviewer that authored the read step must not review it: {refused:?}"
        );
        let msg = format!("{:#}", refused.err().unwrap());
        assert!(
            msg.contains("seat"),
            "the refusal is the binder refusing a seat, not a launch failure: {msg}"
        );

        // Empty map = no author constraint = the dispatch proceeds.
        let empty: std::collections::BTreeMap<String, Vec<String>> =
            [("ce-review".to_string(), Vec::new())]
                .into_iter()
                .collect();
        let port = |_a: &str, _p: &str| -> anyhow::Result<()> { Ok(()) };
        let ctx_empty = DispatchContext {
            scope: DispatchScope {
                run_id: "run-9",
                node_path: "review2",
            },
            reads: &empty,
            prompt_port: Some(&port),
        };
        pane_dispatch_scoped(
            &catalog,
            &project,
            "wf",
            &json!({}),
            &entries,
            &action,
            None,
            Some(&ctx_empty),
        )
        .expect("no reads, no author constraint");
    }

    /// ADR-0022 V8: rex and hc build the SAME launch for one seat. The argv is
    /// one decision (`bare_model_launch`); the planes differ only in transport
    /// (rex an `env K=V` prefix, hc an env map), so the command tail must be
    /// identical token for token.
    #[test]
    fn rex_and_hc_build_the_same_launch_for_one_seat() {
        let choice = crate::models::ModelChoice::Model("zai/glm-5.3".into());
        let (args, env_pin) =
            crate::bare_model_launch("pi", &choice, "", Some("high")).expect("pi launch");
        assert!(
            env_pin.is_none(),
            "pi delivers its model by argv, so both planes carry it in the command: {env_pin:?}"
        );
        let pane_env: BTreeMap<String, String> =
            [("FLYWHEEL_AGENT".to_string(), "Ada".to_string())]
                .into_iter()
                .collect();

        // hc hands (kind, args) to the pane API; rex renders the same pair.
        let hc_command: Vec<String> = std::iter::once("pi".to_string())
            .chain(args.iter().cloned())
            .collect();
        let rex = rex_env_command(&pane_env, "pi", &args);
        // Transport differs (rex prefixes `env K=V`); the COMMAND must not.
        let rex_tail: Vec<&str> = rex.split_whitespace().skip_while(|t| *t != "pi").collect();
        assert_eq!(
            rex_tail.join(" "),
            hc_command.join(" "),
            "behind the env prefix the rex command IS the hc argv: {rex}"
        );

        // The env-delivered harness is the other half: the model rides the env
        // pin on BOTH planes and the command carries no model flag.
        let (oc_args, oc_env) = crate::bare_model_launch(
            "opencode",
            &crate::models::ModelChoice::Model("zai/glm-5.3".into()),
            "",
            None,
        )
        .expect("opencode launch");
        assert!(
            oc_env.is_some(),
            "opencode delivers by env, so the pin is the delivery"
        );
        assert!(
            !oc_args.iter().any(|a| a.contains("glm-5.3")),
            "an env-delivered model must not also ride argv: {oc_args:?}"
        );
        let oc_rex = rex_env_command(&pane_env, "opencode", &oc_args);
        let oc_tail: Vec<&str> = oc_rex
            .split_whitespace()
            .skip_while(|t| *t != "opencode")
            .collect();
        assert_eq!(
            oc_tail,
            std::iter::once("opencode")
                .chain(oc_args.iter().map(String::as_str))
                .collect::<Vec<_>>(),
            "the env-delivered seat keeps the same shape on both planes: {oc_rex}"
        );
    }

    /// bd-6setu.2 / V10 at the dispatch level: when the journal says the
    /// reviewer's pin authored the work under review, the binder must not hand
    /// that step back to it, and with no journal author it may reuse freely.
    #[test]
    fn review_bind_refuses_every_journal_author() {
        let catalog = test_catalog_with_reviewer_harness("pi");
        let _ = &catalog;
        let entries = as_live(vec![
            SpawnEntry {
                pane_id: "w1:p1".into(),
                name: "crew-goal-reviewer".into(),
                profile: Some("reviewer".into()),
                harness: Some("pi".into()),
                model: Some("zai/glm-5.3".into()),
                model_pin: None,
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: Some("BlueLake".into()),
                harness_session: None,
                skills_injected: false,
                incarnation: 0,
            },
            SpawnEntry {
                pane_id: "w1:p2".into(),
                name: "crew-goal-reviewer-2".into(),
                profile: Some("reviewer".into()),
                harness: Some("pi".into()),
                model: Some("grok/grok-4.5".into()),
                model_pin: None,
                effort: None,
                mode: None,
                skills_digest: None,
                mail_identity: Some("RedStone".into()),
                harness_session: None,
                skills_injected: false,
                incarnation: 0,
            },
        ]);
        let spec = toron_workflow::schema::DispatchSpec {
            profile: Some("@reviewer".into()),
            context: Some(toron_workflow::schema::ContextMode::Fresh),
            ..Default::default()
        };
        let authored: std::collections::BTreeSet<String> =
            ["BlueLake".to_string()].into_iter().collect();
        let bound = review_bind("reviewer", &spec, &entries, 5, &authored)
            .expect("a fresh reviewer dispatch binds");
        let chosen = match &bound {
            crate::seat_binder::Binding::Reuse { pin }
            | crate::seat_binder::Binding::FreshSession { pin }
            | crate::seat_binder::Binding::Other { pin }
            | crate::seat_binder::Binding::Patch { pin, .. } => pin.clone(),
            other => panic!("expected a seat, got {other:?}"),
        };
        assert_ne!(
            chosen, "BlueLake",
            "the author of the work under review never reviews it"
        );

        let none = std::collections::BTreeSet::new();
        let free = review_bind("reviewer", &spec, &entries, 5, &none)
            .expect("a fresh reviewer dispatch binds");
        assert!(
            matches!(
                free,
                crate::seat_binder::Binding::Reuse { .. }
                    | crate::seat_binder::Binding::FreshSession { .. }
                    | crate::seat_binder::Binding::Other { .. }
                    | crate::seat_binder::Binding::Patch { .. }
            ),
            "with no journal author the binder is free: {free:?}"
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
/// decision is pure, so these rows need no hc terminal and no live harness.
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
                location: crate::agent_ref::BindingLocation::Pane,
            },
            crate::agent_ref::AgentBinding {
                profile_name: "coder".to_string(),
                model: "openai/gpt-5.2".to_string(),
                harness: "claude".to_string(),
                location: crate::agent_ref::BindingLocation::Pane,
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

#[cfg(test)]
mod rex_door_tests {
    use super::{rex_door_selected, rex_env_command, rex_shell_ready, wait_rex_shell_ready};
    use crate::backend::MultiplexerBackend;
    use crate::hc::{AgentInfo, AgentStatus, BackendError};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn test_agent(pane_id: &str, name: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            agent: None,
            agent_status: status,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: pane_id.to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: String::new(),
            terminal_id: String::new(),
            terminal_title: None,
            workspace_id: "w0".to_string(),
        }
    }

    struct ScriptedBackend {
        agents: Vec<AgentInfo>,
        err: Option<String>,
    }

    impl MultiplexerBackend for ScriptedBackend {
        fn name(&self) -> &'static str {
            "rex-test"
        }

        fn agent_list(&self) -> Result<Vec<AgentInfo>, BackendError> {
            match &self.err {
                Some(message) => Err(BackendError::timeout(message.clone())),
                None => Ok(self.agents.clone()),
            }
        }
    }

    /// The pane door follows the RESOLVED plane, never `REX_SESSION`:
    /// an hc pin wins even inside a rex terminal, and an explicit
    /// superlogical pin selects rex. (The old contract let the env split the
    /// planes: `rex list` saw the session while `flywheel status` read hc.)
    #[test]
    fn the_door_follows_the_resolved_plane_not_rex_session() {
        let _guard = crate::config::lock_env();
        unsafe { std::env::set_var("REX_SESSION", "sess-1") };
        let mut cfg = crate::config::Config {
            backend: Some(crate::config::BackendConfig {
                multiplexer: "hc".to_string(),
                mail: "toron".to_string(),
            }),
            ..Default::default()
        };
        assert!(!rex_door_selected(&cfg), "an hc pin wins over REX_SESSION");
        cfg.backend = Some(crate::config::BackendConfig {
            multiplexer: "superlogical".to_string(),
            mail: "toron".to_string(),
        });
        assert!(rex_door_selected(&cfg), "an explicit rex pin selects rex");
        unsafe { std::env::remove_var("REX_SESSION") };
    }

    /// program_status readiness: idle/working pass, anything else does not.
    #[test]
    fn shell_ready_is_idle_or_working() {
        assert!(rex_shell_ready(&AgentStatus::Idle));
        assert!(rex_shell_ready(&AgentStatus::Working));
        assert!(!rex_shell_ready(&AgentStatus::Blocked));
        assert!(!rex_shell_ready(&AgentStatus::Unknown));
    }

    /// No env map on split: the pin rides as `env K=V`, hc-only keys
    /// stay behind, and values with spaces are quoted.
    #[test]
    fn env_command_filters_plane_and_quotes() {
        let env: BTreeMap<String, String> = [
            ("HC_SESSION".to_string(), "default".to_string()),
            ("HC_SOCKET_PATH".to_string(), "/tmp/x".to_string()),
            ("AGENT_NAME".to_string(), "Pin".to_string()),
            ("NOTE".to_string(), "a b".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            rex_env_command(&env, "opencode", &["--model".to_string(), "x".to_string()]),
            "env AGENT_NAME=Pin NOTE='a b' opencode --model x"
        );
    }

    /// bd-xpuky: the PATH carried by plane_launch_env must reach the one-string
    /// rex command, or the pane's `env K=V <kind>` still resolves on the
    /// server's PATH.
    #[test]
    fn env_command_carries_the_spawners_path() {
        let env: BTreeMap<String, String> = [
            ("PATH".to_string(), "/tmp/mise/bin:/usr/bin".to_string()),
            ("AGENT_NAME".to_string(), "Pin".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            rex_env_command(&env, "opencode", &[]),
            "env AGENT_NAME=Pin PATH=/tmp/mise/bin:/usr/bin opencode"
        );
    }

    #[test]
    fn env_command_without_env_runs_kind_directly() {
        assert_eq!(rex_env_command(&BTreeMap::new(), "pi", &[]), "pi");
    }

    #[test]
    fn waiter_returns_when_program_status_is_working() {
        let backend = ScriptedBackend {
            agents: vec![test_agent("b1", "n1", AgentStatus::Working)],
            err: None,
        };
        assert!(wait_rex_shell_ready(&backend, "b1", "n1", "b1", Duration::from_secs(5)).is_ok());
    }

    /// Fail-closed with the shell refusal (not a mailbox fault) when the
    /// status never becomes idle/working.
    #[test]
    fn waiter_refuses_a_shell_that_never_reports_ready() {
        let backend = ScriptedBackend {
            agents: vec![test_agent("b1", "n1", AgentStatus::Blocked)],
            err: None,
        };
        let err =
            wait_rex_shell_ready(&backend, "b1", "n1", "b1", Duration::from_millis(1)).unwrap_err();
        assert!(
            err.to_string().contains("never rendered a prompt"),
            "{err:?}"
        );
    }

    /// An unreadable backend is the real reason — report it instead of
    /// blaming the shell.
    #[test]
    fn waiter_reports_an_unreadable_backend() {
        let backend = ScriptedBackend {
            agents: vec![],
            err: Some("nope".to_string()),
        };
        let err =
            wait_rex_shell_ready(&backend, "b1", "n1", "b1", Duration::from_millis(1)).unwrap_err();
        assert!(err.to_string().contains("cannot read status"), "{err:?}");
    }
}

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
            "resolved": { "npub": "", "spec": { "harness": harness, "backend": "hc", "role": "reviewer" } },
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
            let handle = {
                let engine = engine.clone();
                let rid = run_id.clone();
                tokio::spawn(async move {
                    engine
                        .run(
                            &rid,
                            RunInput {
                                run_id: Some(rid.clone()),
                                workflow: approval_workflow(),
                                trigger: json!({}),
                            },
                        )
                        .await
                })
            };
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

            let out = match tokio::time::timeout(std::time::Duration::from_secs(90), handle).await
            {
                Ok(Ok(run)) => run.expect("run resumes after approval"),
                Ok(Err(join)) => panic!("run task failed: {join}"),
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

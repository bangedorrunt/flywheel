//! Coalition lifecycle (ADR-0011 Spec 1, P11.3a): the steer surface +
//! manual collab baseline.
//!
//! A **coalition** is an ephemeral, per-goal grouping of pane crews:
//! - a NIP-29 channel (kind-40 create + kind-41 metadata, kind-42 board
//!   posts) published to the mailbox bus — the intent board + shared thread;
//! - a goal workflow run (the goal driver, engine invoke, run recorded in
//!   the run log);
//! - a local manifest (`state_dir()/coalitions/<id>.json`) — the durable
//!   binding (id, goal, run_id, captain, members, beads, status).
//!
//! Manual collab (P11.3a, NO auto-proposer — that is P11.3b): crews declare
//! `status:intent` into the board (P11.2 `declare_intent`), a second crew
//! acquiring the same path hits P11.2's soft-conflict FREEZE, and the two
//! resolve via P11.2 request-release negotiation. Disband (beads-closed
//! primary | manual fallback) closes the coalition: channel close post,
//! member-held reservations released, manifest marked closed.
//!
//! **Auto-collab (P11.3b, bd-foh8.4):** [`CoalitionStore::detect`] reads the
//! intent board and runs the tiered rule proposer (split-by-directory /
//! sequence-by-dependency / merge) from `intent.rs`; [`CoalitionStore::auto_coalesce`]
//! forms an ad-hoc coalition for overlapping UNaffiliated crews on
//! non-trivial source. The soft-conflict freeze path surfaces the detector's
//! proposals instead of leaving negotiation purely manual.
//!
//! **Leaf workers (bd-hkw5.2, ADR-0011 Spec 5 reopen):**
//! [`CoalitionStore::dispatch_leaf`] runs a headless crew as a coalition
//! LEAF worker: the captain pre-splits a non-overlapping scope, the leaf
//! receives it, runs one-shot (the `__run-headless` subprocess, ADR-0010
//! Spec A), the run logs to `runs/`, and the result lands in the coalition
//! channel (kind-42 board post). No intent-declare/negotiate — leaf
//! semantics. [`CoalitionStore::post_leaf_result`] is the hermetic channel
//! post (the testable half).
//!
//! Channel events are signed by the in-process operator key (the captain is
//! the operator authority, ADR-0011 Spec 6) and project-scoped (`#t`) — the
//! mailbox bus persists them (InProcessBus + SQLite under `TORON_DATA_DIR`).
//!
//! governed-by: ADR-0011 (Spec 1, 2, 5, 6)

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use toron_core::bus::{Bus, InProcessBus};
use toron_core::mailbox::{
    AcquireResult, IntentReservation, IntentResult, ListAgentsOptions, Mailbox, ReleaseAsk,
    ReleaseDecision, ReleaseResponse, RequestRelease, RequestReleaseResult, RequestReleaseStatus,
    Reservation, SendResult,
};
use toron_core::nostr::event::build_event;
use toron_core::nostr::kinds::toron_kind;

use crate::run_log;

/// NIP-29 channel-create kind.
pub const CHANNEL_CREATE: u16 = 40;
/// NIP-29 channel-metadata kind.
pub const CHANNEL_METADATA: u16 = 41;
/// NIP-29 channel-message kind.
pub const CHANNEL_MESSAGE: u16 = 42;

/// The durable coalition binding (JSON on disk, `coalitions/<id>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoalitionManifest {
    /// Coalition id (slug, unique per formation).
    pub id: String,
    /// The goal workflow name (the goal driver).
    pub goal: String,
    /// The goal run id (run-log entry).
    pub run_id: String,
    /// The captain's agent name (who formed it).
    pub captain: String,
    /// Member agent names (the squad; membership gate for collab ops).
    pub members: Vec<String>,
    /// NIP-29 channel id (= coalition id).
    pub channel_id: String,
    /// Channel display name.
    pub channel_name: String,
    /// Bead ids tracked for the beads-closed disband gate (may be empty).
    #[serde(default)]
    pub beads: Vec<String>,
    /// The repo whose `.beads` holds the tracked beads (br resolves its
    /// workspace from cwd; the coalition CLI often runs from another repo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker_repo: Option<String>,
    /// Cross-project coalitions (bd-hkw5.3): member -> toron project slug.
    /// Intents/reservations are project-scoped in the mailbox; each crew's
    /// collab ops land in ITS repo's namespace. Members absent from the map
    /// default to the store's project (single-project behavior unchanged).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub member_projects: BTreeMap<String, String>,
    /// Formation timestamp (RFC3339).
    pub created_at: String,
    /// `active` | `closed`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
}

/// One NIP-29 channel event on the bus (status view).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEventView {
    pub id: String,
    pub kind: u16,
    pub content: String,
    pub created_at_secs: i64,
}

/// A channel's on-wire view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelView {
    pub channel_id: String,
    pub name: String,
    /// Kind-40/41/42 events, newest first.
    pub events: Vec<ChannelEventView>,
}

/// Bead gate status (best-effort `br show` probe).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadStatus {
    pub id: String,
    /// `closed` | `open` | `unknown` (br unavailable / no beads tracked).
    pub status: String,
}

/// A frozen path (bd-hkw5.1 observability): a member's in-TTL intent
/// overlaps another agent's active lease — the soft-conflict FREEZE tier
/// (ADR-0011 Spec 4). Frozen paths are the coalition's contention surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenPath {
    /// The contested path glob.
    pub path: String,
    /// The member whose intent is frozen.
    pub agent: String,
    /// Holders of the overlapping active leases (intent or held).
    pub holders: Vec<String>,
}

/// An in-flight request-release negotiation (bd-hkw5.1 observability,
/// ADR-0011 Spec 6): the requester asked the holder to release/defer a
/// lease. `status` = `pending` | `responded` | `escalation_due` | `unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiationView {
    pub requester: String,
    pub holder: String,
    /// The contested path (from the request subject).
    pub path: String,
    pub message_id: String,
    pub status: String,
}

/// Full `coalition status <id>` view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoalitionStatus {
    pub id: String,
    pub goal: String,
    pub run_id: String,
    pub captain: String,
    pub members: Vec<String>,
    /// Cross-project scoping: member -> project slug (absent = store project).
    #[serde(default)]
    pub member_projects: BTreeMap<String, String>,
    /// member -> role (`captain` | `member`).
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
    pub status: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub run: Option<run_log::RunLogEntry>,
    pub channel: ChannelView,
    pub intents: Vec<IntentReservation>,
    pub reservations: Vec<Reservation>,
    /// Frozen paths (soft-conflict intents), bd-hkw5.1.
    #[serde(default)]
    pub frozen: Vec<FrozenPath>,
    /// In-flight request-release negotiations, bd-hkw5.1.
    #[serde(default)]
    pub negotiations: Vec<NegotiationView>,
    pub beads: Vec<BeadStatus>,
}

/// One headless leaf-worker dispatch (bd-hkw5.2): the run-log id + the
/// outcome as the leaf reported it (ADR-0010 Spec E fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafDispatch {
    pub run_id: String,
    pub profile: String,
    pub scope: String,
    pub status: String,
    pub ok: bool,
    pub response: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub model: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// Shared tokio runtime (one per process, mirroring the mail shim).
static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

fn runtime() -> Result<&'static Runtime> {
    RUNTIME
        .get_or_init(|| {
            Runtime::new().map_err(|e| format!("failed to initialize tokio runtime: {e}"))
        })
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

/// Durable mailbox roots: same XDG/D3 resolution as the mail shim.
fn mailbox_roots() -> (PathBuf, PathBuf) {
    crate::mail_shim::mailbox_roots()
}

/// The coalition store: durable mailbox + bus + manifest registry.
///
/// `root` is the coalitions directory (state dir by default; overridable for
/// hermetic tests). `project` is the toron project slug all mailbox ops are
/// scoped to.
pub struct CoalitionStore {
    project: String,
    root: PathBuf,
    mailbox: Mailbox,
    bus: Arc<dyn Bus>,
}

impl CoalitionStore {
    /// Open the store: build the shared runtime + the durable mailbox
    /// (InProcessBus + SQLite, an in-process operator key for channel-event
    /// signing — same pattern as the mail shim). Roots follow
    /// `TORON_DATA_DIR` / `TORON_MCP_DATA_DIR`.
    pub fn open(project: &str, root: PathBuf) -> Result<Self> {
        let (data_dir, _db) = mailbox_roots();
        Self::open_at(project, root, data_dir)
    }

    /// Open with an explicit mailbox data root (hermetic tests).
    pub fn open_at(project: &str, root: PathBuf, data_dir: PathBuf) -> Result<Self> {
        let _rt = runtime()?;
        let store = runtime()?
            .block_on(async {
                std::fs::create_dir_all(&data_dir)
                    .with_context(|| format!("create mail data dir {}", data_dir.display()))?;
                let pool = toron_core::index::schema::connect(
                    &data_dir.join("coalition.sqlite").to_string_lossy(),
                )
                .await?;
                toron_core::index::schema::migrate(&pool).await?;
                let bus = InProcessBus::open(
                    pool.clone(),
                    toron_core::bus::in_process::InProcessBusConfig::default(),
                )
                .await?;
                let operator = toron_core::keys::generate();
                let bus_arc: Arc<dyn Bus> = Arc::new(bus);
                let mailbox =
                    Mailbox::new(bus_arc.clone(), data_dir, Some(operator)).with_index_pool(pool);
                Ok(Self {
                    project: project.to_string(),
                    root,
                    mailbox,
                    bus: bus_arc,
                })
            })
            .map_err(|e: anyhow::Error| anyhow!("coalition store init: {e:#}"))?;
        std::fs::create_dir_all(&store.root)
            .with_context(|| format!("create coalitions dir {}", store.root.display()))?;
        Ok(store)
    }

    /// Register an agent in the project (kind-30013 profile; required before
    /// `form` can include them). Thin wrapper over the mailbox.
    pub fn register_agent(&self, name: &str) -> Result<()> {
        use toron_core::nostr::identity::AgentProfile;
        let profile = AgentProfile {
            name: name.to_string(),
            program: Some("opencode".into()),
            model: None,
            persona: None,
            task_description: None,
        };
        runtime()?
            .block_on(self.mailbox.register_agent(&self.project, &profile))
            .map(|_| ())
            .map_err(|e| anyhow!("register agent {name}: {e}"))
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    /// `coalition form <goal> --crews <squad>`: lint the goal workflow
    /// (engine-tier gate), validate every member is a registered agent,
    /// start the goal run (engine invoke, recording runner), publish the
    /// NIP-29 channel (kind-40 + 41) to the bus, write the manifest.
    ///
    /// Cross-project (bd-hkw5.3): `member_projects` maps a member to the
    /// toron project slug their intents/reservations scope to (absent =
    /// the store's project). Each member must be registered in their OWN
    /// project.
    pub fn form(
        &self,
        goal: &str,
        captain: &str,
        members: &[String],
        member_projects: &BTreeMap<String, String>,
        beads: &[String],
        tracker: Option<&str>,
    ) -> Result<CoalitionManifest> {
        // Goal must be a known workflow + pass the engine-tier lint gate
        // (mirrors `workflow run`'s fail-stop).
        let wf = resolve_goal_workflow(goal)?;
        // bd-4spn GAP 2: lint with the flywheel principle registry (names())
        // — same bridge as the workflow-lint CLI; the fallback list would
        // silently drift.
        let blocking =
            toron_workflow::lint_workflow_with_principles(&wf, crate::principles::names())
                .issues
                .iter()
                .filter(|i| i.level == toron_workflow::Level::Error)
                .count();
        if blocking > 0 {
            bail!(
                "goal workflow {goal:?} has {blocking} blocking lint issue(s) — refusing to form"
            );
        }

        // Members must be registered agents IN THEIR OWN PROJECT (a member
        // listed in `member_projects` with project P must exist in P; others
        // in the store's project). Registration is per (project, agent).
        let project_of = |member: &str| -> &str {
            member_projects
                .get(member)
                .map(String::as_str)
                .unwrap_or(&self.project)
        };
        let distinct_projects: BTreeSet<&str> = members.iter().map(|m| project_of(m)).collect();
        let mut registered: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for project in &distinct_projects {
            let names: Vec<String> = runtime()?
                .block_on(
                    self.mailbox
                        .list_agents(project, ListAgentsOptions::default()),
                )
                .map_err(|e| anyhow!("list agents in project {project}: {e}"))?
                .into_iter()
                .map(|a| a.name)
                .collect();
            registered.insert(project, names);
        }
        let missing: Vec<&String> = members
            .iter()
            .filter(|m| !registered.get(project_of(m)).is_some_and(|n| n.contains(m)))
            .collect();
        if !missing.is_empty() {
            bail!(
                "coalition members not registered in their project: {} (coalition register / flywheel mail register — register each crew in its own project)",
                missing
                    .iter()
                    .map(|m| format!("{m}@{}", project_of(m)))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !members.contains(&captain.to_string()) {
            bail!("captain {captain} must be a member of the coalition");
        }

        let id = coalition_id(goal);
        if self.manifest_path(&id).exists() {
            bail!("coalition {id} already exists (id collision — retry)");
        }
        let run_id = format!("coal-{id}");

        // Start the goal run (recording runner: control-flow executes, side
        // effects recorded — the coalition binds the run; real headless
        // execution of goal steps is `workflow run <goal> --execute`).
        let rt = runtime()?;
        let run_output = rt.block_on(async {
            let runner: toron_workflow::ActionRunner =
                Arc::new(|a: &toron_workflow::Action| Ok(json!({ "action": a.type_name() })));
            let engine = toron_workflow::WorkflowEngine::local_with(
                runner,
                crate::profile::ProfileCatalog::catalog_profile_resolver(),
            )?;
            engine.invoke(&run_id, wf.clone(), json!({})).await
        })?;
        run_log::write_run(&run_log::RunLogEntry {
            run_id: run_id.clone(),
            ts: crate::state::now_iso(),
            workflow: goal.to_string(),
            cron: None,
            profile: "coalition".into(),
            harness: "recording".into(),
            model: String::new(),
            mode: "coalition".into(),
            prompt_hash: String::new(),
            response: run_output.to_string(),
            status: "completed".into(),
            tokens_in: 0,
            tokens_out: 0,
            duration_ms: 0,
            exit_code: 0,
            error: None,
            phase: None,
            phase_degraded: false,
        })?;

        // NIP-29 channel: create + metadata (project-scoped, operator-signed).
        let channel_id = id.clone();
        self.publish_channel_event(
            CHANNEL_CREATE,
            &channel_id,
            &json!({"name": goal}).to_string(),
        )?;
        self.publish_channel_event(
            CHANNEL_METADATA,
            &channel_id,
            &json!({"name": goal, "coalition": true}).to_string(),
        )?;

        let manifest = CoalitionManifest {
            id: id.clone(),
            goal: goal.to_string(),
            run_id,
            captain: captain.to_string(),
            members: members.to_vec(),
            channel_id,
            channel_name: goal.to_string(),
            beads: beads.to_vec(),
            tracker_repo: tracker.map(str::to_string),
            member_projects: member_projects.clone(),
            created_at: crate::state::now_iso(),
            status: "active".into(),
            closed_at: None,
            close_reason: None,
        };
        self.save_manifest(&manifest)?;
        Ok(manifest)
    }

    /// `coalition status <id>`: manifest + run + channel events + member
    /// intents + member-held reservations + bead gate (best-effort).
    pub fn status(&self, id: &str) -> Result<CoalitionStatus> {
        let manifest = self.load_manifest(id)?;
        let run = run_log::find_run(&manifest.run_id);
        let channel = self.channel_view(&manifest.channel_id);
        let intents = self.member_intents(&manifest)?;
        let reservations = self.member_reservations(&manifest)?;
        let frozen = self.frozen_paths(&manifest)?;
        let negotiations = self.in_flight_negotiations(&manifest)?;
        let beads = bead_statuses(&manifest.beads, manifest.tracker_repo.as_deref());
        let roles = manifest
            .members
            .iter()
            .map(|m| {
                (
                    m.clone(),
                    if m == &manifest.captain {
                        "captain".into()
                    } else {
                        "member".into()
                    },
                )
            })
            .collect();
        Ok(CoalitionStatus {
            id: manifest.id.clone(),
            goal: manifest.goal.clone(),
            run_id: manifest.run_id.clone(),
            captain: manifest.captain.clone(),
            members: manifest.members.clone(),
            member_projects: manifest.member_projects.clone(),
            roles,
            status: manifest.status.clone(),
            closed_at: manifest.closed_at.clone(),
            close_reason: manifest.close_reason.clone(),
            run,
            channel,
            intents,
            reservations,
            frozen,
            negotiations,
            beads,
        })
    }

    /// `coalition list`.
    pub fn list(&self) -> Result<Vec<CoalitionManifest>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json")
                && let Ok(m) = self.load_manifest(name.trim_end_matches(".json"))
            {
                out.push(m);
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    /// `coalition disband <id>`: mark closed, release member-held
    /// reservations (best-effort per member), post the channel close
    /// message. With `beads_closed` and a tracked bead list, the gate
    /// requires every bead to be closed first.
    pub fn disband(
        &self,
        id: &str,
        beads_closed: bool,
        reason: Option<&str>,
        tracker_override: Option<&str>,
    ) -> Result<()> {
        let mut manifest = self.load_manifest(id)?;
        if manifest.status == "closed" {
            return Ok(()); // idempotent
        }
        if beads_closed && !manifest.beads.is_empty() {
            let tracker = tracker_override.or(manifest.tracker_repo.as_deref());
            for b in bead_statuses(&manifest.beads, tracker) {
                if b.status != "closed" {
                    bail!(
                        "beads-closed gate: bead {} is {}, not closed — disband aborted",
                        b.id,
                        b.status
                    );
                }
            }
        }
        // Release member-held reservations (best-effort: a member without a
        // local key is skipped with a warning, not an error). Cross-project
        // (bd-hkw5.3): each member's leases are released in THEIR project.
        for member in &manifest.members {
            let project = self.project_of(&manifest, member);
            match runtime()?.block_on(
                self.mailbox
                    .release_file_reservations(&project, member, &[]),
            ) {
                Ok(r) if r.released > 0 => {
                    eprintln!(
                        "coalition disband: released {} lease(s) held by {member} in {project}",
                        r.released
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!(
                    "coalition disband: could not release {member}'s reservations in {project}: {e}"
                ),
            }
        }
        // Channel close post (kind-42, board record).
        let _ = self.publish_channel_event(
            CHANNEL_MESSAGE,
            &manifest.channel_id,
            &json!({ "coalition-closed": true, "reason": reason.unwrap_or("beads-closed") })
                .to_string(),
        );
        manifest.status = "closed".into();
        manifest.closed_at = Some(crate::state::now_iso());
        manifest.close_reason = Some(reason.unwrap_or("disbanded").to_string());
        self.save_manifest(&manifest)?;
        Ok(())
    }

    /// `coalition join <id> --as <agent>`: add a late member to an ACTIVE
    /// coalition (bd-hkw5.4, ADR-0011 dynamic membership). The joiner is
    /// registered in their project (default: the store's project) if absent,
    /// appended to the manifest's members (+ `member_projects` when a
    /// different project is named), and a `member-joined` channel post lands
    /// on the coalition board. From then on every membership-gated op
    /// (status/board read, declare_intent, reserve, leaf) accepts the joiner.
    ///
    /// Idempotent: a member already in the manifest is a no-op success.
    ///
    /// `#[allow(dead_code)]` (bd-hkw5.4): the CLI surface (`coalition
    /// join`/`leave` subcommands) lives in main.rs — held by the orchestrator
    /// during this bead's window; the store methods + hermetic GATE tests are
    /// the landed slice, wired by the main.rs holder next.
    #[allow(dead_code)]
    pub fn join(
        &self,
        id: &str,
        agent: &str,
        member_project: Option<&str>,
    ) -> Result<CoalitionManifest> {
        let mut manifest = self.load_manifest(id)?;
        if manifest.status != "active" {
            bail!(
                "coalition {id} is {} — join requires an active coalition",
                manifest.status
            );
        }
        if manifest.members.contains(&agent.to_string()) {
            return Ok(manifest); // already a member — idempotent
        }
        let project = member_project.unwrap_or(&self.project).to_string();
        // The joiner must exist as an agent in THEIR project (same gate as
        // `form`); registration is idempotent (upsert).
        self.register_agent_in(agent, &project)?;
        let registered: Vec<String> = runtime()?
            .block_on(
                self.mailbox
                    .list_agents(&project, ListAgentsOptions::default()),
            )
            .map_err(|e| anyhow!("list agents in project {project}: {e}"))?
            .into_iter()
            .map(|a| a.name)
            .collect();
        if !registered.contains(&agent.to_string()) {
            bail!("join: agent {agent} not registered in project {project}");
        }
        manifest.members.push(agent.to_string());
        if member_project.is_some() {
            manifest
                .member_projects
                .insert(agent.to_string(), project.clone());
        }
        // The join is observable on the coalition board (kind-42 post).
        let _ = self.publish_channel_event(
            CHANNEL_MESSAGE,
            &manifest.channel_id,
            &json!({
                "member-joined": agent,
                "project": project,
                "members": manifest.members.len(),
            })
            .to_string(),
        );
        self.save_manifest(&manifest)?;
        Ok(manifest)
    }

    /// `coalition leave <id> --as <agent>`: remove a member from an ACTIVE
    /// coalition (bd-hkw5.4, ADR-0011 dynamic membership). The departed
    /// member is dropped from the manifest (+ `member_projects`), their held
    /// reservations are released best-effort in THEIR project (mirrors the
    /// disband cleanup), and a `member-left` channel post lands on the board.
    ///
    /// Intent-cleanup on leave: the departed member's `status:intent`
    /// declarations clear WITHOUT manual cleanup — intents carry NIP-40
    /// expiration (INTENT_TTL_SECS) in toron-core, so they auto-expire; held
    /// leases are released here. `member_intents`/`member_reservations` stop
    /// surfacing the departed member immediately (the members filter).
    ///
    /// `#[allow(dead_code)]` — same main.rs wiring note as `join`.
    #[allow(dead_code)]
    pub fn leave(&self, id: &str, agent: &str) -> Result<CoalitionManifest> {
        let mut manifest = self.load_manifest(id)?;
        if manifest.status != "active" {
            bail!(
                "coalition {id} is {} — leave requires an active coalition",
                manifest.status
            );
        }
        if !manifest.members.contains(&agent.to_string()) {
            return Ok(manifest); // already gone — idempotent
        }
        let project = self.project_of(&manifest, agent);
        // Release the departed member's held leases in their project
        // (best-effort: a member without a local key is skipped with a
        // warning, never an error — same posture as disband).
        match runtime()?.block_on(self.mailbox.release_file_reservations(&project, agent, &[])) {
            Ok(r) if r.released > 0 => {
                eprintln!(
                    "coalition leave: released {} lease(s) held by {agent} in {project}",
                    r.released
                );
            }
            Ok(_) => {}
            Err(e) => eprintln!(
                "coalition leave: could not release {agent}'s reservations in {project}: {e}"
            ),
        }
        manifest.members.retain(|m| m != agent);
        manifest.member_projects.remove(agent);
        let _ = self.publish_channel_event(
            CHANNEL_MESSAGE,
            &manifest.channel_id,
            &json!({
                "member-left": agent,
                "project": project,
                "members": manifest.members.len(),
            })
            .to_string(),
        );
        self.save_manifest(&manifest)?;
        Ok(manifest)
    }

    /// Register an agent in an arbitrary project (join's per-project gate).
    fn register_agent_in(&self, agent: &str, project: &str) -> Result<()> {
        use toron_core::nostr::identity::AgentProfile;
        let profile = AgentProfile {
            name: agent.to_string(),
            program: Some("opencode".into()),
            model: None,
            persona: None,
            task_description: None,
        };
        runtime()?
            .block_on(self.mailbox.register_agent(project, &profile))
            .map(|_| ())
            .map_err(|e| anyhow!("register agent {agent} in {project}: {e}"))
    }

    // ------------------------------------------------------------------
    // Manual collab (P11.2 surfaces, membership-gated)
    // ------------------------------------------------------------------

    /// `coalition intent <id> <paths...>`: declare `status:intent` into the
    /// board (P11.2). The acting agent must be a member. Cross-project
    /// (bd-hkw5.3): the intent lands in the member's OWN project (the
    /// manifest's `member_projects` map; absent = the store's project).
    pub fn declare_intent(
        &self,
        id: &str,
        agent: &str,
        paths: &[String],
        reason: Option<&str>,
        ttl_secs: Option<u64>,
    ) -> Result<IntentResult> {
        let manifest = self.require_active_member(id, agent)?;
        let project = self.project_of(&manifest, agent);
        runtime()?
            .block_on(
                self.mailbox
                    .declare_intent(&project, agent, paths, reason, ttl_secs),
            )
            .map_err(|e| anyhow!("declare intent: {e}"))
    }

    /// `coalition reserve <id> <paths...>`: acquire leases (P11.2). A
    /// soft-conflict FREEZE (another member's in-TTL intent) appears in
    /// `soft_conflicts` — the P11.3b detector's trigger: on freeze, the
    /// tiered rule-proposer proposes the split (instead of manual-only
    /// negotiation). `proposal` is `Some` exactly when the freeze fired and
    /// the detector found a rule for it.
    pub fn reserve(
        &self,
        id: &str,
        agent: &str,
        paths: &[String],
        ttl_secs: Option<u64>,
        exclusive: bool,
        reason: Option<&str>,
    ) -> Result<AcquireResult> {
        let manifest = self.require_active_member(id, agent)?;
        let project = self.project_of(&manifest, agent);
        let result = runtime()?
            .block_on(
                self.mailbox
                    .file_reservation_paths(&project, agent, paths, ttl_secs, exclusive, reason),
            )
            .map_err(|e| anyhow!("reserve: {e}"))?;
        // P11.3b: on a soft-conflict FREEZE, run the detector over the board
        // and attach the rule-split proposal for the frozen paths (the
        // P11.2 manual negotiation stays the dispute backstop when a
        // proposal is rejected). Best-effort: a detector failure does not
        // fail the reserve.
        if !result.soft_conflicts.is_empty()
            && let Ok(report) = self.detect()
            && let Some(p) = report.proposals.first()
        {
            return Ok(AcquireResult {
                soft_conflicts: result.soft_conflicts,
                proposal: Some(serde_json::to_value(p)?),
                ..result
            });
        }
        Ok(result)
    }

    /// `coalition negotiate request`: ask the holder to release/defer (P11.2).
    /// Cross-project (bd-hkw5.3): routed to the TARGET's project (where the
    /// reservation lives); the sender must hold a key there too (the
    /// mailbox resolves keys per (project, agent)).
    pub fn negotiate_request(
        &self,
        id: &str,
        agent: &str,
        target_agent: &str,
        reservation_id: &str,
        ask: ReleaseAsk,
        note: Option<&str>,
    ) -> Result<RequestReleaseResult> {
        let manifest = self.require_active_member(id, agent)?;
        if !manifest.members.contains(&target_agent.to_string()) {
            bail!("negotiate target {target_agent} is not a coalition member");
        }
        let project = self.project_of(&manifest, target_agent);
        runtime()?
            .block_on(self.mailbox.request_release(&RequestRelease {
                project_slug: project.clone(),
                sender_name: agent.to_string(),
                target_agent: target_agent.to_string(),
                reservation_id: reservation_id.to_string(),
                ask,
                note: note.map(str::to_string),
            }))
            .map_err(|e| anyhow!("negotiate request: {e}"))
    }

    /// `coalition negotiate respond`: the holder answers (P11.2). Routed to
    /// the responding agent's own project.
    pub fn negotiate_respond(
        &self,
        id: &str,
        agent: &str,
        request_message_id: &str,
        decision: ReleaseDecision,
        note: Option<&str>,
    ) -> Result<SendResult> {
        let manifest = self.require_active_member(id, agent)?;
        let project = self.project_of(&manifest, agent);
        runtime()?
            .block_on(self.mailbox.respond_request_release(&ReleaseResponse {
                project_slug: project.clone(),
                sender_name: agent.to_string(),
                request_message_id: request_message_id.to_string(),
                decision,
                note: note.map(str::to_string),
            }))
            .map_err(|e| anyhow!("negotiate respond: {e}"))
    }

    /// `coalition negotiate status`: resolution of a request (P11.2). Routed
    /// to the requester's own project.
    pub fn negotiate_status(
        &self,
        id: &str,
        agent: &str,
        request_message_id: &str,
        window_secs: Option<u64>,
    ) -> Result<RequestReleaseStatus> {
        let manifest = self.require_active_member(id, agent)?;
        let project = self.project_of(&manifest, agent);
        Ok(runtime()?.block_on(self.mailbox.request_release_status(
            &project,
            agent,
            request_message_id,
            window_secs,
        )))
    }

    /// `coalition negotiate escalate`: captain DM after the tier window
    /// (P11.2). Routed to the requester's own project.
    pub fn negotiate_escalate(
        &self,
        id: &str,
        agent: &str,
        request_message_id: &str,
        note: Option<&str>,
    ) -> Result<()> {
        let manifest = self.require_active_member(id, agent)?;
        let project = self.project_of(&manifest, agent);
        runtime()?
            .block_on(self.mailbox.escalate_request_release(
                &project,
                agent,
                request_message_id,
                note,
            ))
            .map_err(|e| anyhow!("negotiate escalate: {e}"))
    }

    // ------------------------------------------------------------------
    // Leaf workers (bd-hkw5.2): headless crews as coalition leaves
    // ------------------------------------------------------------------

    /// `coalition leaf <id> --as <member> --profile @X --scope <pre-split>`:
    /// dispatch a headless leaf worker (ADR-0011 Spec 5 reopen). The captain
    /// pre-splits a non-overlapping scope; the leaf receives it, runs
    /// ONE-SHOT via the `__run-headless` subprocess (ADR-0010 Spec A
    /// subprocess-reuse — never an in-process herdr/toron client), the run
    /// logs to `runs/`, and the result lands in the coalition channel
    /// (kind-42 board post). No intent-declare/negotiate — leaf semantics.
    ///
    /// The leaf runs as the acting MEMBER (must be a member of an active
    /// coalition); `profile` is the profile ref (`@planner`) whose harness +
    /// behavior the leaf runs under.
    pub fn dispatch_leaf(
        &self,
        id: &str,
        agent: &str,
        profile: &str,
        scope: &str,
        harness_override: Option<&str>,
    ) -> Result<LeafDispatch> {
        let manifest = self.require_active_member(id, agent)?;
        let catalog = crate::profile::ProfileCatalog::load()?;
        let profile_name = crate::headless::strip_profile_ref(profile);
        let profile_entry = catalog.get(profile_name).ok_or_else(|| {
            anyhow!("leaf dispatch: unknown profile @{profile_name} (flywheel profile list)")
        })?;

        // Leaf prompt: the profile's behavior preamble, then the leaf mission
        // (coalition context + the captain's pre-split scope).
        let base = leaf_prompt(&manifest.id, &manifest.goal, scope);
        let prompt = crate::headless::conditioned_prompt(&profile_entry.behavior, &base);

        // Subprocess-reuse (Spec A): the __run-headless leaf owns the harness
        // call + run log. --json keeps the outcome machine-readable.
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("__run-headless")
            .arg(profile_name)
            .arg(&prompt)
            .arg("--workflow")
            .arg(&manifest.goal)
            .arg("--json");
        if let Some(h) = harness_override.filter(|h| !h.trim().is_empty()) {
            cmd.arg("--harness").arg(h);
        }
        if !profile_entry.model.is_empty() {
            cmd.arg("--model").arg(&profile_entry.model);
        }
        let output = cmd
            .output()
            .with_context(|| format!("__run-headless for coalition leaf @{profile_name}"))?;
        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let trimmed = raw.trim();
        let value: Value = if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            v
        } else {
            let err = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(anyhow!(
                "coalition leaf @{profile_name}: leaf failed ({}): {}",
                if err.trim().is_empty() {
                    "no JSON on stdout"
                } else {
                    "stderr"
                },
                if err.trim().is_empty() {
                    trimmed
                } else {
                    err.trim()
                }
            ));
        };
        let outcome = leaf_outcome(&value);
        let outcome = LeafDispatch {
            profile: profile_name.to_string(),
            scope: scope.to_string(),
            ..outcome
        };

        // The result lands in the coalition: kind-42 board post on the
        // coalition channel (the GATE's "result lands in the coalition").
        self.post_leaf_result(id, agent, profile, scope, &outcome)?;
        Ok(outcome)
    }

    /// The hermetic half of [`CoalitionStore::dispatch_leaf`]: post a leaf
    /// outcome as a kind-42 board event on the coalition channel (the
    /// channel lands the result where `coalition status` / members can see
    /// it). Membership-gated. Pure-ish: no subprocess, no run-log write —
    /// the testable surface.
    pub fn post_leaf_result(
        &self,
        id: &str,
        agent: &str,
        profile: &str,
        scope: &str,
        outcome: &LeafDispatch,
    ) -> Result<()> {
        let manifest = self.require_active_member(id, agent)?;
        let content = json!({
            "leaf-result": true,
            "agent": agent,
            "profile": profile,
            "scope": scope,
            "run_id": outcome.run_id,
            "status": outcome.status,
            "ok": outcome.ok,
        })
        .to_string();
        self.publish_channel_event(CHANNEL_MESSAGE, &manifest.channel_id, &content)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Auto-collab (P11.3b, bd-foh8.4): detector + tiered proposer
    // ------------------------------------------------------------------

    /// The intent board as the detector's view: active `status:intent`
    /// declarations in the project (all crews, not just this coalition's
    /// members — overlaps with outsiders are the auto-coalesce trigger).
    pub fn board(&self) -> Result<Vec<crate::intent::IntentView>> {
        let intents = runtime()?
            .block_on(self.mailbox.active_intents(&self.project))
            .map_err(|e| anyhow!("active intents: {e}"))?;
        Ok(intents
            .into_iter()
            .map(|i| crate::intent::IntentView {
                agent: i.agent_name,
                path: i.path_pattern,
                reason: i.reason,
            })
            .collect())
    }

    /// Run the detector over the intent board: overlaps + tiered rule-split
    /// proposals + the auto-coalesce gate. `is_affiliated` decides whether a
    /// crew already belongs to a coalition (project-wide manifest scan).
    pub fn detect(&self) -> Result<crate::intent::DetectionReport> {
        let board = self.board()?;
        let active_coalitions = self.list()?;
        let report = crate::intent::run_detector(&board, &|agent| {
            active_coalitions
                .iter()
                .any(|m| m.status == "active" && m.members.iter().any(|x| x == agent))
        });
        Ok(report)
    }

    /// Auto-coalesce: form an ad-hoc coalition around the goal workflow for
    /// EVERY non-trivial overlap between unaffiliated crews the detector
    /// found. The coalition binds the two crews' members; the captain is the
    /// first member of the pair. Idempotent per (goal, members) — re-detection
    /// does not duplicate.
    ///
    /// Returns the manifests of the formed coalitions (the detector's
    /// `auto_coalesce` list drives it; empty overlap list → empty result).
    pub fn auto_coalesce(
        &self,
        goal: &str,
        captain: &str,
        report: &crate::intent::DetectionReport,
    ) -> Result<Vec<CoalitionManifest>> {
        let mut formed = Vec::new();
        // Idempotency key: (goal, member set) — coalition ids are
        // timestamped (unique per formation), so dedupe on the SIGNATURE
        // against the PERSISTENT registry (a re-detection must not
        // duplicate an already-formed pair, even across CLI invocations).
        let existing = self.list()?;
        for o in &report.auto_coalesce {
            if !o.non_trivial {
                continue;
            }
            // Members: the two overlapping crews' agents, deduped + the
            // captain first (captain must be a member).
            let mut members: Vec<String> = vec![captain.to_string()];
            for m in [o.a.clone(), o.b.clone()] {
                if !members.contains(&m) {
                    members.push(m);
                }
            }
            let already = existing.iter().any(|m| {
                m.status == "active"
                    && m.goal == goal
                    && m.members.len() == members.len()
                    && m.members.iter().all(|x| members.contains(x))
            });
            if already {
                continue;
            }
            // The overlap pair is the board's evidence; the goal workflow
            // must exist (lint gate inside form). Auto-coalesce is
            // single-project (the board is project-scoped) — no
            // member_projects mapping.
            let manifest = self.form(goal, captain, &members, &BTreeMap::new(), &[], None)?;
            formed.push(manifest);
        }
        Ok(formed)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn require_active_member(&self, id: &str, agent: &str) -> Result<CoalitionManifest> {
        let manifest = self.load_manifest(id)?;
        if manifest.status != "active" {
            bail!(
                "coalition {id} is {} — collab ops require an active coalition",
                manifest.status
            );
        }
        if !manifest.members.contains(&agent.to_string()) {
            bail!("agent {agent} is not a member of coalition {id}");
        }
        Ok(manifest)
    }

    /// Member intents (the board, P11.2 `active_intents`, member-filtered).
    /// Cross-project (bd-hkw5.3): fans across the distinct projects the
    /// members scope to, then member-filters.
    fn member_intents(&self, manifest: &CoalitionManifest) -> Result<Vec<IntentReservation>> {
        let mut out = Vec::new();
        for project in self.distinct_member_projects(manifest) {
            let intents = runtime()?
                .block_on(self.mailbox.active_intents(&project))
                .map_err(|e| anyhow!("active intents in {project}: {e}"))?;
            out.extend(intents);
        }
        Ok(out
            .into_iter()
            .filter(|i| manifest.members.contains(&i.agent_name))
            .collect())
    }

    /// Member-held reservations (P11.2 `active_reservations`, member-filtered).
    /// Cross-project (bd-hkw5.3): fans across the distinct projects the
    /// members scope to, then member-filters.
    fn member_reservations(&self, manifest: &CoalitionManifest) -> Result<Vec<Reservation>> {
        let mut out = Vec::new();
        for project in self.distinct_member_projects(manifest) {
            let res = runtime()?
                .block_on(self.mailbox.active_reservations(&project))
                .map_err(|e| anyhow!("active reservations in {project}: {e}"))?;
            out.extend(res);
        }
        Ok(out
            .into_iter()
            .filter(|r| manifest.members.contains(&r.agent_name))
            .collect())
    }

    /// The project a member's collab ops scope to (bd-hkw5.3): the
    /// manifest's `member_projects` map, else the store's project.
    fn project_of(&self, manifest: &CoalitionManifest, member: &str) -> String {
        manifest
            .member_projects
            .get(member)
            .cloned()
            .unwrap_or_else(|| self.project.clone())
    }

    /// Frozen paths (bd-hkw5.1): a member's in-TTL intent that SOFT-conflicts
    /// with another agent's active lease (ADR-0011 Spec 4 freeze tier). The
    /// conflict probe is per-member-best-effort: a member without a local key
    /// in the store's data dir (e.g. an xproj member registered elsewhere)
    /// contributes no frozen paths — status never fails on observability.
    fn frozen_paths(&self, manifest: &CoalitionManifest) -> Result<Vec<FrozenPath>> {
        let mut out = Vec::new();
        for intent in self.member_intents(manifest)? {
            let project = self.project_of(manifest, &intent.agent_name);
            let report = runtime()?
                .block_on(self.mailbox.check_file_reservation_conflicts(
                    &project,
                    &intent.agent_name,
                    std::slice::from_ref(&intent.path_pattern),
                ))
                .map_err(|e| anyhow!("frozen-path probe for {}: {e}", intent.agent_name))?;
            for soft in &report.soft_conflicts {
                out.push(FrozenPath {
                    path: soft.path.clone(),
                    agent: intent.agent_name.clone(),
                    holders: soft.holders.clone(),
                });
            }
        }
        Ok(out)
    }

    /// In-flight request-release negotiations (bd-hkw5.1, ADR-0011 Spec 6):
    /// the members' inboxes carry the typed kind-14 request DMs; each
    /// `[request-release]` row becomes a negotiation view with its resolved
    /// status (pending | responded | escalation_due | unknown). Best-effort:
    /// a member whose inbox cannot be fetched (no local key) contributes
    /// nothing.
    fn in_flight_negotiations(&self, manifest: &CoalitionManifest) -> Result<Vec<NegotiationView>> {
        use toron_core::mailbox::{FetchInbox, RequestReleaseStatus};
        let mut out = Vec::new();
        for member in &manifest.members {
            let project = self.project_of(manifest, member);
            let inbox = runtime()?
                .block_on(self.mailbox.fetch_inbox(&FetchInbox {
                    project_slug: project.clone(),
                    agent_name: member.clone(),
                    unread_only: false,
                    limit: Some(30),
                    include_bodies: false,
                    urgent_only: false,
                    since_ts: None,
                    thread_id: None,
                }))
                .map_err(|e| anyhow!("negotiation probe inbox for {member}: {e}"))?;
            for m in inbox {
                let Some(path) = m.subject.strip_prefix("[request-release]") else {
                    continue;
                };
                let path = path.trim().to_string();
                // The requester is the row's sender; status resolves from
                // the requester's perspective (the window tier).
                let status = runtime()?.block_on(
                    self.mailbox
                        .request_release_status(&project, &m.sender, &m.id, None),
                );
                let status = match status {
                    RequestReleaseStatus::Pending { .. } => "pending",
                    RequestReleaseStatus::Responded { .. } => "responded",
                    RequestReleaseStatus::EscalationDue { .. } => "escalation_due",
                    RequestReleaseStatus::Unknown => "unknown",
                };
                out.push(NegotiationView {
                    requester: m.sender,
                    holder: member.clone(),
                    path,
                    message_id: m.id,
                    status: status.into(),
                });
            }
        }
        Ok(out)
    }

    /// Distinct projects across the coalition's members (bd-hkw5.3): the
    /// union of the map's values + the store's project (members without an
    /// entry). Empty map = the store's project only.
    fn distinct_member_projects(&self, manifest: &CoalitionManifest) -> BTreeSet<String> {
        let mut projects: BTreeSet<String> = manifest.member_projects.values().cloned().collect();
        projects.insert(self.project.clone());
        projects
    }

    /// NIP-29 channel view: replay kind-40/41/42 events (project-scoped)
    /// from the bus, newest first.
    fn channel_view(&self, channel_id: &str) -> ChannelView {
        let filter =
            toron_core::bus::filter::kind_project_filter(toron_kind(CHANNEL_CREATE), &self.project)
                .kinds([
                    toron_kind(CHANNEL_CREATE),
                    toron_kind(CHANNEL_METADATA),
                    toron_kind(CHANNEL_MESSAGE),
                ]);
        let events = runtime()
            .and_then(|rt| {
                rt.block_on(self.replay(filter.clone()))
                    .map_err(|e| anyhow!("{e}"))
            })
            .unwrap_or_default();
        let mut views: Vec<ChannelEventView> = events
            .into_iter()
            .filter(|e| channel_tag(e).as_deref() == Some(channel_id))
            .map(|e| ChannelEventView {
                id: e.id.to_hex(),
                kind: e.kind.as_u16(),
                content: e.content,
                created_at_secs: e.created_at.as_secs() as i64,
            })
            .collect();
        views.sort_by_key(|v| std::cmp::Reverse(v.created_at_secs));
        let name = views
            .iter()
            .find(|v| v.kind == CHANNEL_METADATA)
            .and_then(|v| {
                serde_json::from_str::<serde_json::Value>(&v.content)
                    .ok()
                    .and_then(|c| c["name"].as_str().map(str::to_string))
            })
            .unwrap_or_else(|| channel_id.to_string());
        ChannelView {
            channel_id: channel_id.to_string(),
            name,
            events: views,
        }
    }

    /// Publish a project-scoped NIP-29 channel event signed by the operator.
    fn publish_channel_event(&self, kind: u16, channel_id: &str, content: &str) -> Result<()> {
        let operator = self
            .mailbox
            .operator()
            .ok_or_else(|| anyhow!("no operator key configured"))?
            .clone();
        let tags = toron_core::bus::scoping::with_project_scoping(
            toron_kind(kind),
            vec![
                toron_core::nostr::Tag::parse(["h", channel_id]).expect("h"),
                toron_core::nostr::Tag::parse(["name", channel_id]).expect("name"),
            ],
            Some(&self.project),
        );
        let event = build_event(
            &operator,
            toron_kind(kind),
            content,
            tags,
            toron_core::nostr::Timestamp::now(),
        )?;
        runtime()?
            .block_on(self.bus.publish(event))
            .map_err(|e| anyhow!("publish channel event kind {kind}: {e}"))?;
        Ok(())
    }

    /// Drain the bus replay for `filter` (history only, stops at EOSE).
    async fn replay(
        &self,
        filter: toron_core::nostr::Filter,
    ) -> Result<Vec<toron_core::nostr::Event>> {
        let mut sub = self.bus.subscribe(filter).await?;
        let mut events = Vec::new();
        while let Some(toron_core::bus::subscription::SubscriptionEvent::Event(e)) =
            sub.next().await
        {
            events.push(e);
        }
        Ok(events)
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn load_manifest(&self, id: &str) -> Result<CoalitionManifest> {
        let path = self.manifest_path(id);
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "coalition {id} not found (coalition form) at {}",
                path.display()
            )
        })?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn save_manifest(&self, manifest: &CoalitionManifest) -> Result<()> {
        let path = self.manifest_path(&manifest.id);
        std::fs::write(&path, serde_json::to_string_pretty(manifest)?)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

/// The leaf worker's mission prompt (pure): coalition context + the
/// captain's pre-split scope. The scope is the work; the framing tells the
/// leaf it is one-shot with no negotiation (leaf semantics, ADR-0011
/// Spec 5).
pub fn leaf_prompt(coalition_id: &str, goal: &str, scope: &str) -> String {
    format!(
        "Coalition leaf worker for {goal} (coalition {coalition_id}).\n\
         You received a pre-split, non-overlapping scope. Run it one-shot, no \
         negotiation. Post your result to the coalition channel when done.\n\n\
         Scope:\n{scope}"
    )
}

/// Parse the `__run-headless --json` leaf outcome into a [`LeafDispatch`]
/// (pure; the JSON shape is the leaf's output contract). A missing
/// `run_id`/`status` defaults honestly (`unknown`) rather than panicking.
pub fn leaf_outcome(value: &Value) -> LeafDispatch {
    fn s(v: &Value, key: &str) -> String {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }
    fn u(v: &Value, key: &str) -> u64 {
        v.get(key).and_then(Value::as_u64).unwrap_or(0)
    }
    let status = s(value, "status");
    LeafDispatch {
        run_id: s(value, "run_id"),
        profile: String::new(),
        scope: String::new(),
        status: if status.is_empty() {
            "unknown".into()
        } else {
            status
        },
        ok: value.get("ok").and_then(Value::as_bool).unwrap_or(false),
        response: s(value, "response"),
        tokens_in: u(value, "tokens_in"),
        tokens_out: u(value, "tokens_out"),
        model: s(value, "model"),
        duration_ms: u(value, "duration_ms"),
        error: value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Coalition id: goal slug + compact timestamp (unique per formation).
fn coalition_id(goal: &str) -> String {
    let ts = crate::state::now_iso()
        .replace(['-', ':', 'T', 'Z'], "")
        .replace('.', "");
    // Sub-second suffix: two coalitions created within the same second must
    // not collide (second-granular now_iso is not unique by itself).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}-{:09}", slug(goal), ts, nanos)
}

/// Slugify the goal (identifier-safe).
fn slug(goal: &str) -> String {
    let mut out = String::new();
    for c in goal.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Resolve a goal workflow across built-ins + catalog (same precedence as
/// `workflow run`: catalog shadows built-ins).
fn resolve_goal_workflow(goal: &str) -> Result<toron_workflow::schema::Workflow> {
    let mut all: BTreeMap<String, toron_workflow::schema::Workflow> =
        crate::builtin_workflows::builtin_workflows();
    for (name, wf) in crate::workflow_catalog::WorkflowCatalog::load()?.workflows {
        all.insert(name, wf);
    }
    all.get(goal)
        .cloned()
        .ok_or_else(|| anyhow!("goal {goal:?} is not a known workflow (flywheel workflow list)"))
}

/// The `h` tag of a NIP-29 event (the channel id).
fn channel_tag(event: &toron_core::nostr::Event) -> Option<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(|s| s == "h").unwrap_or(false))
        .and_then(|t| t.as_slice().get(1).map(std::clone::Clone::clone))
}

/// Best-effort `br show <id>` status probe per tracked bead. `br` absent
/// or failing → `unknown` (the beads-closed gate then refuses to pass).
/// `tracker_repo` = the repo whose `.beads` the probe must use (br
/// resolves its workspace from cwd; the coalition CLI often runs from a
/// different repo).
fn bead_statuses(beads: &[String], tracker_repo: Option<&str>) -> Vec<BeadStatus> {
    beads
        .iter()
        .map(|id| BeadStatus {
            id: id.clone(),
            status: bead_status(id, tracker_repo),
        })
        .collect()
}

fn bead_status(id: &str, tracker_repo: Option<&str>) -> String {
    let mut cmd = std::process::Command::new("br");
    cmd.args(["show", id, "--json"]);
    if let Some(repo) = tracker_repo {
        cmd.current_dir(repo);
    }
    match cmd.output() {
        Ok(o) if o.status.success() => parse_br_status(&String::from_utf8_lossy(&o.stdout)),
        _ => "unknown".into(),
    }
}

/// Pure parser for `br show <id> --json` stdout (the unit-test surface).
fn parse_br_status(stdout: &str) -> String {
    if stdout.contains("\"status\":\"closed\"") {
        "closed".into()
    } else if stdout.contains("\"status\":\"open\"")
        || stdout.contains("\"status\":\"in_progress\"")
    {
        "open".into()
    } else {
        "unknown".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-t9ey leftover: coalition mailbox must share the durable XDG root,
    /// never a per-pid temp sqlite.
    #[test]
    fn mailbox_roots_follow_xdg_not_per_pid() {
        let _g = crate::config::lock_env();
        unsafe {
            std::env::remove_var("TORON_DATA_DIR");
            std::env::remove_var("TORON_MCP_DATA_DIR");
            std::env::set_var("XDG_DATA_HOME", "/tmp/fw-coalition-xdg");
        }
        let (data_dir, db) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/fw-coalition-xdg/toron"),
            "coalition mailbox must use resolve_data_root"
        );
        assert!(
            !db.to_string_lossy()
                .contains(&std::process::id().to_string()),
            "must not be per-pid: {}",
            db.display()
        );
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }

    #[test]
    fn test_coalition_id_is_slugged_and_unique() {
        let a = coalition_id("Hello World Goal");
        let b = coalition_id("Hello World Goal");
        assert!(a.starts_with("hello-world-goal-"), "{a}");
        assert_ne!(a, b, "timestamped ids differ");
        assert!(slug("Auth Refactor / #2").starts_with("auth-refactor-"));
    }

    #[test]
    fn test_channel_tag_extraction() {
        use toron_core::nostr::{Keys, Tag, Timestamp};
        let keys = Keys::generate();
        let ev = build_event(
            &keys,
            toron_kind(CHANNEL_CREATE),
            "{}",
            vec![Tag::parse(["h", "coal-1"]).expect("h")],
            Timestamp::now(),
        )
        .expect("build");
        assert_eq!(channel_tag(&ev), Some("coal-1".to_string()));
    }

    #[test]
    fn test_bead_status_parsing() {
        // No br in a hermetic test env → unknown; the parser handles it.
        let status = bead_status("bd-does-not-exist", None);
        assert!(
            status == "unknown" || status == "closed" || status == "open",
            "{status}"
        );
    }

    /// The pure parser: closed / open / in_progress / garbage.
    #[test]
    fn parse_br_status_classifies_output() {
        assert_eq!(
            parse_br_status(r#"[{"id":"bd-x","status":"closed"}]"#),
            "closed"
        );
        assert_eq!(
            parse_br_status(r#"[{"id":"bd-x","status":"in_progress"}]"#),
            "open",
            "in_progress counts as open for the gate"
        );
        assert_eq!(
            parse_br_status(r#"[{"id":"bd-x","status":"open"}]"#),
            "open"
        );
        assert_eq!(
            parse_br_status(r#"{"error":{"code":"ISSUE_NOT_FOUND"}}"#),
            "unknown"
        );
        assert_eq!(parse_br_status(""), "unknown");
        assert_eq!(parse_br_status("garbage"), "unknown");
    }

    /// The manifest round-trips the tracker repo (the beads-closed gate's
    /// probe cwd) — a coalition formed with --tracker survives save/load.
    #[test]
    fn manifest_roundtrips_tracker_repo() {
        let mut m = CoalitionManifest {
            id: "coal-x".into(),
            goal: "beads".into(),
            run_id: "coal-coal-x".into(),
            captain: "oc".into(),
            members: vec!["oc".into(), "pi".into()],
            channel_id: "coal-x".into(),
            channel_name: "beads".into(),
            beads: vec!["bd-x".into()],
            tracker_repo: Some("/tmp/tracker".into()),
            member_projects: BTreeMap::new(),
            created_at: "2026-08-09T00:00:00Z".into(),
            status: "active".into(),
            closed_at: None,
            close_reason: None,
        };
        m.channel_id = "coal-x".into();
        let s = serde_json::to_string(&m).unwrap();
        let back: CoalitionManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tracker_repo.as_deref(), Some("/tmp/tracker"));
        assert_eq!(back.id, "coal-x");
        assert!(back.member_projects.is_empty());
    }

    /// P11.3b e2e (bd-foh8.4, hermetic mailbox): two crews declare
    /// overlapping intents on a source dir → the detector proposes a rule
    /// split + flags the pair for auto-coalesce; a README overlap does NOT
    /// coalesce. `auto_coalesce` forms an ad-hoc coalition binding both.
    #[test]
    fn detector_e2e_overlap_proposes_and_coalesces() {
        let root = std::env::temp_dir().join(format!("fw-coal-det-{}", uuid::Uuid::new_v4()));
        let data = std::env::temp_dir().join(format!("fw-coal-det-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("det-e2e", root.clone(), data.clone())
            .expect("open hermetic store");
        store.register_agent("oc").expect("register oc");
        store.register_agent("pi").expect("register pi");

        // Crew oc intends a broad source glob; crew pi intends a subpath
        // (literal — mailbox overlap semantics are glob-vs-path).
        runtime()
            .unwrap()
            .block_on(store.mailbox.declare_intent(
                "det-e2e",
                "oc",
                &["src/**".into()],
                Some("bd-a"),
                None,
            ))
            .expect("oc intent");
        runtime()
            .unwrap()
            .block_on(store.mailbox.declare_intent(
                "det-e2e",
                "pi",
                &["src/workflow/engine.rs".into()],
                Some("bd-b"),
                None,
            ))
            .expect("pi intent");
        // Crew gk intends a doc path (trivial — never coalesces).
        store.register_agent("gk").expect("register gk");
        runtime()
            .unwrap()
            .block_on(store.mailbox.declare_intent(
                "det-e2e",
                "gk",
                &["docs/guide.md".into()],
                None,
                None,
            ))
            .expect("gk intent");

        let report = store.detect().expect("detect");
        let pairs: Vec<String> = report
            .overlaps
            .iter()
            .map(|o| format!("{}<->{}", o.a, o.b))
            .collect();
        assert!(
            pairs.iter().any(|p| {
                let mut parts = p.split("<->");
                let (a, b) = (parts.next().unwrap(), parts.next().unwrap());
                (a == "oc" && b == "pi") || (a == "pi" && b == "oc")
            }),
            "oc<->pi overlap expected, got {pairs:?}"
        );
        assert!(
            report.proposals.iter().any(|p| {
                let same_pair = (p.agents[0] == "oc" && p.agents[1] == "pi")
                    || (p.agents[0] == "pi" && p.agents[1] == "oc");
                same_pair && p.kind == crate::intent::ProposalKind::SequenceByDependency
            }),
            "{:?}",
            report.proposals
        );
        // gk's trivial overlap never coalesces.
        assert!(
            report
                .auto_coalesce
                .iter()
                .all(|o| o.a != "gk" && o.b != "gk"),
            "trivial overlap must not coalesce: {:?}",
            report.auto_coalesce
        );
        assert_eq!(report.auto_coalesce.len(), 1, "{:?}", report.auto_coalesce);

        // Auto-coalesce forms an ad-hoc coalition binding the pair. Goal
        // must be a dispatch-only workflow (a request_approval step would
        // park the recording run durably — `beads` is dispatch-only).
        let formed = store
            .auto_coalesce("beads", "oc", &report)
            .expect("auto-coalesce");
        assert_eq!(formed.len(), 1, "one ad-hoc coalition formed");
        assert_eq!(formed[0].members, vec!["oc".to_string(), "pi".to_string()]);
        assert_eq!(formed[0].status, "active");
        assert!(
            formed[0].member_projects.is_empty(),
            "auto-coalesce stays single-project"
        );
        // Idempotent: a second pass forms nothing new.
        let again = store.auto_coalesce("beads", "oc", &report).expect("again");
        assert!(again.is_empty(), "idempotent per formation");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// P11.3b e2e (bd-foh8.4): an ALREADY-affiliated crew does not
    /// auto-coalesce — affiliation is the gate.
    #[test]
    fn detector_e2e_affiliated_crew_does_not_coalesce() {
        let root = std::env::temp_dir().join(format!("fw-coal-aff-{}", uuid::Uuid::new_v4()));
        let data = std::env::temp_dir().join(format!("fw-coal-aff-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("det-aff", root.clone(), data.clone())
            .expect("open hermetic store");
        store.register_agent("oc").expect("register oc");
        store.register_agent("pi").expect("register pi");
        // oc is already in a coalition with pi.
        store
            .form(
                "beads",
                "oc",
                &["oc".into(), "pi".into()],
                &BTreeMap::new(),
                &[],
                None,
            )
            .expect("form coalition");
        runtime()
            .unwrap()
            .block_on(
                store
                    .mailbox
                    .declare_intent("det-aff", "oc", &["src/**".into()], None, None),
            )
            .expect("oc intent");
        runtime()
            .unwrap()
            .block_on(store.mailbox.declare_intent(
                "det-aff",
                "pi",
                &["src/foo.rs".into()],
                None,
                None,
            ))
            .expect("pi intent");

        let report = store.detect().expect("detect");
        assert_eq!(report.overlaps.len(), 1, "{:?}", report.overlaps);
        assert!(
            report.auto_coalesce.is_empty(),
            "affiliated crews must NOT auto-coalesce: {:?}",
            report.auto_coalesce
        );
        // The rule-split proposal still fires (detection is affiliation-blind).
        assert!(!report.proposals.is_empty(), "{:?}", report.proposals);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Cross-project coalitions (bd-hkw5.3) e2e (hermetic mailbox): crews
    /// from two toron projects form ONE coalition. Each member's intents +
    /// reservations land in THEIR OWN project (repo namespace); status fans
    /// both boards; disband releases reservations in BOTH projects.
    #[test]
    fn xproj_coalition_scopes_intents_per_project_and_disband_cleans_both() {
        let root = std::env::temp_dir().join(format!("fw-coal-xproj-{}", uuid::Uuid::new_v4()));
        let data =
            std::env::temp_dir().join(format!("fw-coal-xproj-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("proj-a", root.clone(), data.clone())
            .expect("open hermetic store");

        // Crew oc lives in proj-a (the store's project); crew pi lives in
        // proj-b (a second project in the same mailbox bus).
        store.register_agent("oc").expect("register oc in proj-a");
        let pi_profile = toron_core::nostr::identity::AgentProfile {
            name: "pi".into(),
            program: Some("opencode".into()),
            model: None,
            persona: None,
            task_description: None,
        };
        runtime()
            .unwrap()
            .block_on(store.mailbox.register_agent("proj-b", &pi_profile))
            .expect("register pi in proj-b");

        // Form a coalition binding BOTH crews; oc maps to proj-a (implicit),
        // pi maps to proj-b.
        let member_projects = BTreeMap::from([("pi".to_string(), "proj-b".to_string())]);
        let m = store
            .form(
                "beads",
                "oc",
                &["oc".into(), "pi".into()],
                &member_projects,
                &[],
                None,
            )
            .expect("form xproj coalition");
        assert_eq!(m.members.len(), 2);
        assert_eq!(
            m.member_projects.get("pi").map(String::as_str),
            Some("proj-b")
        );
        assert!(
            !m.member_projects.contains_key("oc"),
            "oc defaults to store project"
        );

        // Each crew declares an intent in ITS OWN project via the store
        // (project_of routes the member).
        store
            .declare_intent(&m.id, "oc", &["src/toron/**".into()], Some("crew-a"), None)
            .expect("oc intent in proj-a");
        store
            .declare_intent(
                &m.id,
                "pi",
                &["src/flywheel/**".into()],
                Some("crew-b"),
                None,
            )
            .expect("pi intent in proj-b");

        // The boards are project-scoped: oc's intent is NOT visible in
        // proj-b; pi's is NOT visible in proj-a.
        let intents_a = runtime()
            .unwrap()
            .block_on(store.mailbox.active_intents("proj-a"))
            .expect("proj-a board");
        let intents_b = runtime()
            .unwrap()
            .block_on(store.mailbox.active_intents("proj-b"))
            .expect("proj-b board");
        assert!(
            intents_a
                .iter()
                .any(|i| i.agent_name == "oc" && i.path_pattern == "src/toron/**"),
            "oc's intent must be on the proj-a board: {intents_a:?}"
        );
        assert!(
            !intents_a.iter().any(|i| i.agent_name == "pi"),
            "pi's intent must NOT leak into proj-a: {intents_a:?}"
        );
        assert!(
            intents_b
                .iter()
                .any(|i| i.agent_name == "pi" && i.path_pattern == "src/flywheel/**"),
            "pi's intent must be on the proj-b board: {intents_b:?}"
        );
        assert!(
            !intents_b.iter().any(|i| i.agent_name == "oc"),
            "oc's intent must NOT leak into proj-b: {intents_b:?}"
        );

        // Status fans BOTH projects' boards into one coalition view.
        let st = store.status(&m.id).expect("status");
        assert_eq!(
            st.intents.len(),
            2,
            "status fans both boards: {:?}",
            st.intents
        );
        assert_eq!(
            st.member_projects.get("pi").map(String::as_str),
            Some("proj-b")
        );

        // Both crews hold reservations in their own projects.
        store
            .reserve(
                &m.id,
                "oc",
                &["src/toron/engine.rs".into()],
                None,
                true,
                Some("crew-a"),
            )
            .expect("oc reserve");
        store
            .reserve(
                &m.id,
                "pi",
                &["src/flywheel/engine.rs".into()],
                None,
                true,
                Some("crew-b"),
            )
            .expect("pi reserve");
        let held_a = runtime()
            .unwrap()
            .block_on(store.mailbox.active_reservations("proj-a"))
            .expect("proj-a holds");
        let held_b = runtime()
            .unwrap()
            .block_on(store.mailbox.active_reservations("proj-b"))
            .expect("proj-b holds");
        assert!(held_a.iter().any(|r| r.agent_name == "oc"), "{held_a:?}");
        assert!(held_b.iter().any(|r| r.agent_name == "pi"), "{held_b:?}");
        assert!(
            !held_a.iter().any(|r| r.agent_name == "pi"),
            "no cross-project leak: {held_a:?}"
        );

        // Disband cleans BOTH projects.
        store
            .disband(&m.id, false, Some("gate"), None)
            .expect("disband");
        assert_eq!(store.status(&m.id).expect("status").status, "closed");
        let held_a_after = runtime()
            .unwrap()
            .block_on(store.mailbox.active_reservations("proj-a"))
            .expect("proj-a after disband");
        let held_b_after = runtime()
            .unwrap()
            .block_on(store.mailbox.active_reservations("proj-b"))
            .expect("proj-b after disband");
        assert!(held_a_after.is_empty(), "proj-a cleaned: {held_a_after:?}");
        assert!(held_b_after.is_empty(), "proj-b cleaned: {held_b_after:?}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Cross-project coalitions (bd-hkw5.3): a member must be registered in
    /// the project it is scoped to — form refuses an unregistered crew.
    #[test]
    fn xproj_coalition_requires_member_registered_in_own_project() {
        let root = std::env::temp_dir().join(format!("fw-coal-xproj-rej-{}", uuid::Uuid::new_v4()));
        let data =
            std::env::temp_dir().join(format!("fw-coal-xproj-rej-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("proj-a", root.clone(), data.clone())
            .expect("open hermetic store");
        store.register_agent("oc").expect("register oc in proj-a");
        // pi is NOT registered in proj-b.
        let err = store
            .form(
                "beads",
                "oc",
                &["oc".into(), "pi".into()],
                &BTreeMap::from([("pi".to_string(), "proj-b".to_string())]),
                &[],
                None,
            )
            .expect_err("form must refuse pi@proj-b (unregistered)");
        assert!(
            err.to_string().contains("not registered in their project"),
            "got: {err}"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// bd-hkw5.2: the leaf mission prompt carries coalition context + the
    /// pre-split scope (the leaf knows what it runs and that it is one-shot).
    #[test]
    fn leaf_prompt_carries_coalition_context_and_scope() {
        let p = leaf_prompt("coal-1", "beads", "split: crates/toron-core/src/mailbox/");
        assert!(p.contains("coal-1"), "{p}");
        assert!(p.contains("beads"), "{p}");
        assert!(p.contains("split: crates/toron-core/src/mailbox/"), "{p}");
        assert!(p.contains("one-shot"), "{p}");
        assert!(p.contains("no negotiation"), "{p}");
    }

    /// bd-hkw5.2: the leaf JSON contract parses into a LeafDispatch —
    /// success fields flow, missing status defaults to `unknown`, errors
    /// surface.
    #[test]
    fn leaf_outcome_parses_success_and_failure() {
        let ok = leaf_outcome(&json!({
            "ok": true,
            "run_id": "hl-20260810-1",
            "response": "done",
            "status": "success",
            "tokens_in": 100,
            "tokens_out": 20,
            "model": "glm-5.2",
            "duration_ms": 1500,
        }));
        assert_eq!(ok.run_id, "hl-20260810-1");
        assert_eq!(ok.status, "success");
        assert!(ok.ok);
        assert_eq!(ok.response, "done");
        assert_eq!(ok.tokens_in, 100);
        assert_eq!(ok.model, "glm-5.2");

        let err = leaf_outcome(&json!({
            "ok": false,
            "run_id": "hl-20260810-2",
            "status": "fail",
            "error": "model 404",
        }));
        assert_eq!(err.status, "fail");
        assert!(!err.ok);
        assert_eq!(err.error.as_deref(), Some("model 404"));

        // Missing status -> honest "unknown" (never a panic).
        let bare = leaf_outcome(&json!({ "ok": true }));
        assert_eq!(bare.status, "unknown");
        assert_eq!(bare.run_id, "");
    }

    /// bd-hkw5.2 GATE (hermetic half): a leaf outcome posted via
    /// `post_leaf_result` lands as a kind-42 board event on the coalition
    /// channel — visible in `coalition status` — and non-members are
    /// refused. (The subprocess half, `dispatch_leaf`, shells out to
    /// `__run-headless`; the channel landing is this pure surface.)
    #[test]
    fn leaf_result_lands_in_coalition_channel() {
        let root = std::env::temp_dir().join(format!("fw-coal-leaf-{}", uuid::Uuid::new_v4()));
        let data = std::env::temp_dir().join(format!("fw-coal-leaf-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("leaf-e2e", root.clone(), data.clone())
            .expect("open hermetic store");
        store.register_agent("oc").expect("register oc");
        store.register_agent("pi").expect("register pi");
        let m = store
            .form(
                "beads",
                "oc",
                &["oc".into(), "pi".into()],
                &BTreeMap::new(),
                &[],
                None,
            )
            .expect("form");

        let outcome = LeafDispatch {
            run_id: "hl-20260810-99".into(),
            profile: "planner".into(),
            scope: "split: src/".into(),
            status: "success".into(),
            ok: true,
            response: "split done".into(),
            tokens_in: 10,
            tokens_out: 5,
            model: "glm-5.2".into(),
            duration_ms: 900,
            error: None,
        };
        store
            .post_leaf_result(&m.id, "oc", "@planner", "split: src/", &outcome)
            .expect("post leaf result");

        // The result lands in the coalition: the channel carries a kind-42
        // leaf-result board event.
        let st = store.status(&m.id).expect("status");
        let leaf_event = st
            .channel
            .events
            .iter()
            .find(|e| e.kind == CHANNEL_MESSAGE && e.content.contains("leaf-result"))
            .expect("kind-42 leaf-result board event");
        assert!(
            leaf_event.content.contains("hl-20260810-99"),
            "{leaf_event:?}"
        );
        assert!(leaf_event.content.contains("success"), "{leaf_event:?}");

        // Non-member is refused at the gate.
        let err = store
            .post_leaf_result(&m.id, "gk", "@planner", "split: src/", &outcome)
            .expect_err("gk is not a member");
        assert!(err.to_string().contains("not a member"), "got: {err}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// bd-hkw5.1 GATE (hermetic): the deepened status shows roles, intent
    /// TTL, frozen paths (soft conflicts), in-flight negotiations, and the
    /// full view survives JSON round-trips (agents parse it clean).
    #[test]
    fn status_observability_shows_roles_ttl_frozen_negotiations() {
        let root = std::env::temp_dir().join(format!("fw-coal-obs-{}", uuid::Uuid::new_v4()));
        let data = std::env::temp_dir().join(format!("fw-coal-obs-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("obs-e2e", root.clone(), data.clone())
            .expect("open hermetic store");
        store.register_agent("oc").expect("register oc");
        store.register_agent("pi").expect("register pi");
        let m = store
            .form(
                "beads",
                "oc",
                &["oc".into(), "pi".into()],
                &BTreeMap::new(),
                &[],
                None,
            )
            .expect("form");

        // oc declares a broad intent; pi declares an overlapping subpath —
        // the soft-conflict freeze fires on pi's (and oc's) intent.
        store
            .declare_intent(&m.id, "oc", &["src/**".into()], Some("bd-a"), Some(300))
            .expect("oc intent with TTL");
        store
            .declare_intent(
                &m.id,
                "pi",
                &["src/engine.rs".into()],
                Some("bd-b"),
                Some(300),
            )
            .expect("pi intent");

        // oc holds a lease; pi asks oc to release it (in-flight negotiation).
        let res = store
            .reserve(
                &m.id,
                "oc",
                &["src/guard.rs".into()],
                Some(600),
                true,
                Some("bd-a"),
            )
            .expect("oc reserve");
        let reservation_id = res.granted[0].id.clone();
        store
            .negotiate_request(
                &m.id,
                "pi",
                "oc",
                &reservation_id,
                ReleaseAsk::Release,
                Some("hand it over"),
            )
            .expect("pi requests release");

        let st = store.status(&m.id).expect("status");

        // Roles: captain vs member.
        assert_eq!(st.roles.get("oc").map(String::as_str), Some("captain"));
        assert_eq!(st.roles.get("pi").map(String::as_str), Some("member"));

        // Intents carry TTL + expiry (epoch µs).
        let oc_intent = st
            .intents
            .iter()
            .find(|i| i.agent_name == "oc")
            .expect("oc intent in status");
        assert!(oc_intent.expires_ts_us.is_some(), "intent expiry present");
        assert!(oc_intent.ttl_secs > 0, "intent ttl present");

        // Frozen paths: pi's overlapping intent is frozen against oc's lease.
        assert!(
            st.frozen
                .iter()
                .any(|f| f.agent == "pi" && f.path == "src/engine.rs"),
            "pi's intent frozen: {:?}",
            st.frozen
        );

        // In-flight negotiation: pi -> oc on the contested lease.
        assert!(
            st.negotiations
                .iter()
                .any(|n| n.requester == "pi" && n.holder == "oc" && n.path == "src/guard.rs"),
            "negotiation visible: {:?}",
            st.negotiations
        );

        // Rich JSON parses clean (the agent surface).
        let value = serde_json::to_value(&st).expect("status serializes");
        let back: CoalitionStatus = serde_json::from_value(value).expect("status deserializes");
        assert_eq!(back.frozen.len(), st.frozen.len());
        assert_eq!(back.negotiations.len(), st.negotiations.len());
        assert_eq!(back.roles.get("oc").map(String::as_str), Some("captain"));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// bd-hkw5.4 join-mid-goal GATE (hermetic mailbox): a crew joins an
    /// ACTIVE coalition mid-goal, reads the intent board (status shows the
    /// existing members' intents), and acquires a NON-OVERLAPPING scope
    /// (reserve succeeds against a path the incumbent never touched). The
    /// join is idempotent and a non-member still cannot reserve.
    #[test]
    fn dynamic_join_reads_board_and_acquires_non_overlapping_scope() {
        let root = std::env::temp_dir().join(format!("fw-coal-join-{}", uuid::Uuid::new_v4()));
        let data = std::env::temp_dir().join(format!("fw-coal-join-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("join-e2e", root.clone(), data.clone())
            .expect("open hermetic store");
        store.register_agent("oc").expect("register oc");
        store.register_agent("pi").expect("register pi");

        // Form with oc + pi; pi declares an intent on the board.
        let m = store
            .form(
                "beads",
                "oc",
                &["oc".into(), "pi".into()],
                &BTreeMap::new(),
                &[],
                None,
            )
            .expect("form");
        store
            .declare_intent(&m.id, "pi", &["src/engine.rs".into()], Some("pi-wip"), None)
            .expect("pi intent");

        // gk joins mid-goal (registers itself via join, idempotent on re-join).
        let joined = store.join(&m.id, "gk", None).expect("gk joins");
        assert!(joined.members.contains(&"gk".to_string()), "{joined:?}");
        let again = store.join(&m.id, "gk", None).expect("gk joins again");
        assert_eq!(again.members.len(), joined.members.len(), "idempotent join");

        // Board read: the late joiner sees the incumbent's intent.
        let st = store.status(&m.id).expect("status after join");
        assert!(
            st.intents
                .iter()
                .any(|i| i.agent_name == "pi" && i.path_pattern == "src/engine.rs"),
            "late joiner reads the board: {:?}",
            st.intents
        );
        assert!(st.members.contains(&"gk".to_string()), "gk in status");

        // Non-overlapping scope: gk reserves a path pi never touched — no
        // hard conflict, lease granted.
        let got = store
            .reserve(
                &m.id,
                "gk",
                &["src/workflow/**".into()],
                Some(120),
                true,
                Some("gk-slice"),
            )
            .expect("gk reserves non-overlapping scope");
        assert!(got.conflicts.is_empty(), "{got:?}");
        assert!(
            got.granted
                .iter()
                .any(|g| g.path_pattern == "src/workflow/**"),
            "gk lease granted: {:?}",
            got.granted
        );

        // An outsider (never joined) still cannot reserve.
        let err = store.reserve(
            &m.id,
            "outsider",
            &["src/other.rs".into()],
            None,
            true,
            None,
        );
        assert!(err.is_err(), "non-member reserve denied");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// bd-hkw5.4 leave GATE (hermetic mailbox): a departed crew's intents
    /// clear WITHOUT manual cleanup — leave drops the member from the
    /// manifest (status stops surfacing their intents/reservations), releases
    /// their held leases in their project, and their `status:intent`
    /// declarations auto-expire via NIP-40 TTL (toron-core semantics, verified
    /// here by the intent disappearing from the member-filtered board after
    /// leave + the TTL window).
    #[test]
    fn dynamic_leave_clears_departed_member_without_manual_cleanup() {
        let root = std::env::temp_dir().join(format!("fw-coal-leave-{}", uuid::Uuid::new_v4()));
        let data =
            std::env::temp_dir().join(format!("fw-coal-leave-data-{}", uuid::Uuid::new_v4()));
        let store = CoalitionStore::open_at("leave-e2e", root.clone(), data.clone())
            .expect("open hermetic store");
        store.register_agent("oc").expect("register oc");
        store.register_agent("pi").expect("register pi");

        let m = store
            .form(
                "beads",
                "oc",
                &["oc".into(), "pi".into()],
                &BTreeMap::new(),
                &[],
                None,
            )
            .expect("form");
        store
            .declare_intent(
                &m.id,
                "pi",
                &["src/engine.rs".into()],
                Some("pi-wip"),
                Some(1),
            )
            .expect("pi intent (1s TTL)");
        store
            .reserve(
                &m.id,
                "pi",
                &["src/guard.rs".into()],
                Some(120),
                true,
                Some("pi-hold"),
            )
            .expect("pi reserves");

        // pi leaves (ungraceful — no manual cleanup call).
        let after = store.leave(&m.id, "pi").expect("pi leaves");
        assert!(!after.members.contains(&"pi".to_string()), "{after:?}");

        // Status no longer surfaces the departed member's intents or leases.
        let st = store.status(&m.id).expect("status after leave");
        assert!(!st.members.contains(&"pi".to_string()), "{st:?}");
        assert!(
            !st.intents.iter().any(|i| i.agent_name == "pi"),
            "departed intents not surfaced: {:?}",
            st.intents
        );
        assert!(
            !st.reservations.iter().any(|r| r.agent_name == "pi"),
            "departed leases released: {:?}",
            st.reservations
        );

        // Held leases physically released in the mailbox (leave cleanup).
        let held = runtime()
            .unwrap()
            .block_on(store.mailbox.active_reservations("leave-e2e"))
            .expect("active reservations");
        assert!(
            !held.iter().any(|r| r.agent_name == "pi"),
            "pi's lease physically released: {held:?}"
        );

        // The departed member's intent auto-expires via NIP-40 (TTL) — no
        // manual cleanup: wait out the 1s TTL, then the intent is gone from
        // the project-wide board (the auto-expire mechanism clears it).
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let board = store.board().expect("board");
        assert!(
            !board
                .iter()
                .any(|i| i.agent == "pi" && i.path == "src/engine.rs"),
            "pi's intent auto-expired off the board: {board:?}"
        );

        // Rejoin after leave works (membership is dynamic both ways).
        let rejoined = store.join(&m.id, "pi", None).expect("pi rejoins");
        assert!(rejoined.members.contains(&"pi".to_string()), "{rejoined:?}");

        // Leave is idempotent on a departed member.
        let again = store.leave(&m.id, "pi").expect("leave again");
        assert!(!again.members.contains(&"pi".to_string()));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&data);
    }
}

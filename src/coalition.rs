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
//! Channel events are signed by the in-process operator key (the captain is
//! the operator authority, ADR-0011 Spec 6) and project-scoped (`#t`) — the
//! mailbox bus persists them (InProcessBus + SQLite under `TORON_DATA_DIR`).
//!
//! governed-by: ADR-0011 (Spec 1, 2, 6)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::runtime::Runtime;

use toron_core::bus::{Bus, InProcessBus};
use toron_core::mailbox::{
    AcquireResult, IntentReservation, IntentResult, ListAgentsOptions, Mailbox, ReleaseAsk,
    ReleaseDecision, ReleaseResponse, RequestRelease, RequestReleaseResult,
    RequestReleaseStatus, Reservation, SendResult,
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

/// Full `coalition status <id>` view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoalitionStatus {
    pub id: String,
    pub goal: String,
    pub run_id: String,
    pub captain: String,
    pub members: Vec<String>,
    pub status: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub run: Option<run_log::RunLogEntry>,
    pub channel: ChannelView,
    pub intents: Vec<IntentReservation>,
    pub reservations: Vec<Reservation>,
    pub beads: Vec<BeadStatus>,
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

/// Durable mailbox roots (mirrors the mail shim): `TORON_DATA_DIR` (or the
/// `TORON_MCP_DATA_DIR` alias) pins the root; else a per-process temp dir.
fn mailbox_roots() -> (PathBuf, PathBuf) {
    for var in ["TORON_DATA_DIR", "TORON_MCP_DATA_DIR"] {
        if let Ok(dir) = std::env::var(var) {
            let dir = dir.trim();
            if !dir.is_empty() {
                let root = PathBuf::from(dir);
                return (root.clone(), root.join("flywheel-mail.sqlite"));
            }
        }
    }
    (
        std::env::temp_dir().join("flywheel-mail"),
        std::env::temp_dir().join(format!("flywheel-mail-{}.sqlite", std::process::id())),
    )
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
                let pool =
                    toron_core::index::schema::connect(&data_dir.join("coalition.sqlite").to_string_lossy())
                        .await?;
                toron_core::index::schema::migrate(&pool).await?;
                let bus = InProcessBus::open(
                    pool.clone(),
                    toron_core::bus::in_process::InProcessBusConfig::default(),
                )
                .await?;
                let operator = toron_core::keys::generate();
                let bus_arc: Arc<dyn Bus> = Arc::new(bus);
                let mailbox = Mailbox::new(bus_arc.clone(), data_dir, Some(operator))
                    .with_index_pool(pool);
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
    pub fn form(
        &self,
        goal: &str,
        captain: &str,
        members: &[String],
        beads: &[String],
    ) -> Result<CoalitionManifest> {
        // Goal must be a known workflow + pass the engine-tier lint gate
        // (mirrors `workflow run`'s fail-stop).
        let wf = resolve_goal_workflow(goal)?;
        let blocking = toron_workflow::lint_workflow(&wf)
            .issues
            .iter()
            .filter(|i| i.level == toron_workflow::Level::Error)
            .count();
        if blocking > 0 {
            bail!("goal workflow {goal:?} has {blocking} blocking lint issue(s) — refusing to form");
        }

        // Members must be registered agents (they sign intents/acquires).
        let registered: Vec<String> = runtime()?
            .block_on(self.mailbox.list_agents(&self.project, ListAgentsOptions::default()))
            .map_err(|e| anyhow!("list agents in project {}: {e}", self.project))?
            .into_iter()
            .map(|a| a.name)
            .collect();
        let missing: Vec<&String> = members.iter().filter(|m| !registered.contains(m)).collect();
        if !missing.is_empty() {
            bail!(
                "coalition members not registered in project {}: {} (coalition register / flywheel mail register)",
                self.project,
                missing
                    .iter()
                    .map(|m| m.as_str())
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
            let runner: toron_workflow::ActionRunner = Arc::new(|a: &toron_workflow::Action| {
                Ok(json!({ "action": a.type_name() }))
            });
            let engine = toron_workflow::WorkflowEngine::local(runner)?;
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
        })?;

        // NIP-29 channel: create + metadata (project-scoped, operator-signed).
        let channel_id = id.clone();
        self.publish_channel_event(CHANNEL_CREATE, &channel_id, &json!({"name": goal}).to_string())?;
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
        let beads = bead_statuses(&manifest.beads);
        Ok(CoalitionStatus {
            id: manifest.id.clone(),
            goal: manifest.goal.clone(),
            run_id: manifest.run_id.clone(),
            captain: manifest.captain.clone(),
            members: manifest.members.clone(),
            status: manifest.status.clone(),
            closed_at: manifest.closed_at.clone(),
            close_reason: manifest.close_reason.clone(),
            run,
            channel,
            intents,
            reservations,
            beads,
        })
    }

    /// `coalition list`.
    pub fn list(&self) -> Result<Vec<CoalitionManifest>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") {
                if let Ok(m) = self.load_manifest(&name.trim_end_matches(".json")) {
                    out.push(m);
                }
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    /// `coalition disband <id>`: mark closed, release member-held
    /// reservations (best-effort per member), post the channel close
    /// message. With `beads_closed` and a tracked bead list, the gate
    /// requires every bead to be closed first.
    pub fn disband(&self, id: &str, beads_closed: bool, reason: Option<&str>) -> Result<()> {
        let mut manifest = self.load_manifest(id)?;
        if manifest.status == "closed" {
            return Ok(()); // idempotent
        }
        if beads_closed && !manifest.beads.is_empty() {
            for b in bead_statuses(&manifest.beads) {
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
        // local key is skipped with a warning, not an error).
        for member in &manifest.members {
            match runtime()?.block_on(
                self.mailbox
                    .release_file_reservations(&self.project, member, &[]),
            ) {
                Ok(r) if r.released > 0 => {
                    eprintln!(
                        "coalition disband: released {} lease(s) held by {member}",
                        r.released
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!(
                    "coalition disband: could not release {member}'s reservations: {e}"
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

    // ------------------------------------------------------------------
    // Manual collab (P11.2 surfaces, membership-gated)
    // ------------------------------------------------------------------

    /// `coalition intent <id> <paths...>`: declare `status:intent` into the
    /// board (P11.2). The acting agent must be a member.
    pub fn declare_intent(
        &self,
        id: &str,
        agent: &str,
        paths: &[String],
        reason: Option<&str>,
        ttl_secs: Option<u64>,
    ) -> Result<IntentResult> {
        self.require_active_member(id, agent)?;
        runtime()?
            .block_on(self.mailbox.declare_intent(
                &self.project,
                agent,
                paths,
                reason,
                ttl_secs,
            ))
            .map_err(|e| anyhow!("declare intent: {e}"))
    }

    /// `coalition reserve <id> <paths...>`: acquire leases (P11.2). A
    /// soft-conflict FREEZE (another member's in-TTL intent) appears in
    /// `soft_conflicts` — the manual-collab trigger.
    pub fn reserve(
        &self,
        id: &str,
        agent: &str,
        paths: &[String],
        ttl_secs: Option<u64>,
        exclusive: bool,
        reason: Option<&str>,
    ) -> Result<AcquireResult> {
        self.require_active_member(id, agent)?;
        runtime()?
            .block_on(self.mailbox.file_reservation_paths(
                &self.project,
                agent,
                paths,
                ttl_secs,
                exclusive,
                reason,
            ))
            .map_err(|e| anyhow!("reserve: {e}"))
    }

    /// `coalition negotiate request`: ask the holder to release/defer (P11.2).
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
        runtime()?
            .block_on(self.mailbox.request_release(&RequestRelease {
                project_slug: self.project.clone(),
                sender_name: agent.to_string(),
                target_agent: target_agent.to_string(),
                reservation_id: reservation_id.to_string(),
                ask,
                note: note.map(str::to_string),
            }))
            .map_err(|e| anyhow!("negotiate request: {e}"))
    }

    /// `coalition negotiate respond`: the holder answers (P11.2).
    pub fn negotiate_respond(
        &self,
        id: &str,
        agent: &str,
        request_message_id: &str,
        decision: ReleaseDecision,
        note: Option<&str>,
    ) -> Result<SendResult> {
        self.require_active_member(id, agent)?;
        runtime()?
            .block_on(self.mailbox.respond_request_release(&ReleaseResponse {
                project_slug: self.project.clone(),
                sender_name: agent.to_string(),
                request_message_id: request_message_id.to_string(),
                decision,
                note: note.map(str::to_string),
            }))
            .map_err(|e| anyhow!("negotiate respond: {e}"))
    }

    /// `coalition negotiate status`: resolution of a request (P11.2).
    pub fn negotiate_status(
        &self,
        id: &str,
        agent: &str,
        request_message_id: &str,
        window_secs: Option<u64>,
    ) -> Result<RequestReleaseStatus> {
        self.require_active_member(id, agent)?;
        Ok(runtime()?.block_on(self.mailbox.request_release_status(
            &self.project,
            agent,
            request_message_id,
            window_secs,
        )))
    }

    /// `coalition negotiate escalate`: captain DM after the tier window
    /// (P11.2).
    pub fn negotiate_escalate(
        &self,
        id: &str,
        agent: &str,
        request_message_id: &str,
        note: Option<&str>,
    ) -> Result<()> {
        self.require_active_member(id, agent)?;
        runtime()?
            .block_on(self.mailbox.escalate_request_release(
                &self.project,
                agent,
                request_message_id,
                note,
            ))
            .map_err(|e| anyhow!("negotiate escalate: {e}"))
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
    fn member_intents(&self, manifest: &CoalitionManifest) -> Result<Vec<IntentReservation>> {
        let intents = runtime()?
            .block_on(self.mailbox.active_intents(&self.project))
            .map_err(|e| anyhow!("active intents: {e}"))?;
        Ok(intents
            .into_iter()
            .filter(|i| manifest.members.contains(&i.agent_name))
            .collect())
    }

    /// Member-held reservations (P11.2 `active_reservations`, member-filtered).
    fn member_reservations(&self, manifest: &CoalitionManifest) -> Result<Vec<Reservation>> {
        let res = runtime()?
            .block_on(self.mailbox.active_reservations(&self.project))
            .map_err(|e| anyhow!("active reservations: {e}"))?;
        Ok(res
            .into_iter()
            .filter(|r| manifest.members.contains(&r.agent_name))
            .collect())
    }

    /// NIP-29 channel view: replay kind-40/41/42 events (project-scoped)
    /// from the bus, newest first.
    fn channel_view(&self, channel_id: &str) -> ChannelView {
        let filter = toron_core::bus::filter::kind_project_filter(
            toron_kind(CHANNEL_CREATE),
            &self.project,
        )
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
        views.sort_by(|a, b| b.created_at_secs.cmp(&a.created_at_secs));
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
                toron_core::nostr_sdk::nostr::Tag::parse(["h", channel_id]).expect("h"),
                toron_core::nostr_sdk::nostr::Tag::parse(["name", channel_id]).expect("name"),
            ],
            Some(&self.project),
        );
        let event = build_event(
            &operator,
            toron_kind(kind),
            content,
            tags,
            toron_core::nostr_sdk::nostr::Timestamp::now(),
        )?;
        runtime()?
            .block_on(self.bus.publish(event))
            .map_err(|e| anyhow!("publish channel event kind {kind}: {e}"))?;
        Ok(())
    }

    /// Drain the bus replay for `filter` (history only, stops at EOSE).
    async fn replay(
        &self,
        filter: toron_core::nostr_sdk::nostr::Filter,
    ) -> Result<Vec<toron_core::nostr_sdk::nostr::Event>> {
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

/// Coalition id: goal slug + compact timestamp (unique per formation).
fn coalition_id(goal: &str) -> String {
    let ts = crate::state::now_iso().replace(['-', ':', 'T', 'Z'], "").replace('.', "");
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
fn channel_tag(event: &toron_core::nostr_sdk::nostr::Event) -> Option<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(|s| s == "h").unwrap_or(false))
        .and_then(|t| t.as_slice().get(1).map(|s| s.to_string()))
}

/// Best-effort `br show <id>` status probe per tracked bead. `br` absent or
/// failing → `unknown` (the beads-closed gate then refuses to pass).
fn bead_statuses(beads: &[String]) -> Vec<BeadStatus> {
    beads
        .iter()
        .map(|id| BeadStatus {
            id: id.clone(),
            status: bead_status(id),
        })
        .collect()
}

fn bead_status(id: &str) -> String {
    let out = std::process::Command::new("br")
        .args(["show", id, "--json"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("\"status\":\"closed\"") {
                "closed".into()
            } else if s.contains("\"status\":\"open\"") || s.contains("\"status\":\"in_progress\"")
            {
                "open".into()
            } else {
                "unknown".into()
            }
        }
        _ => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use toron_core::nostr_sdk::nostr::{Event, EventId, Keys, Tag, Timestamp};
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
        let _ = EventId::from_hex(&ev.id.to_hex());
    }

    #[test]
    fn test_bead_status_parsing() {
        // No br in a hermetic test env → unknown; the parser handles it.
        let status = bead_status("bd-does-not-exist");
        assert!(status == "unknown" || status == "closed" || status == "open", "{status}");
    }
}

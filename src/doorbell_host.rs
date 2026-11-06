// governed-by: ADR-0012
//! Embedded host binding (bd-asr2.5): [`RtHost`] implements
//! `doorbell_rt::Host` over the live flywheel machinery — `workspace::tick`
//! as the decider, `handle_workspace_cmds` as the effect layer, the spawn
//! registry / mailbox / hc shims as producers. [`run_embedded`] wires
//! sensors around a `doorbell_rt::Brain` and runs it to shutdown.
//!
//! One implementation: every arm below mirrors `cmd_internal_doorbell`
//! (removed); behavioral deltas are marked DELTA with their reason.

use crate::config::Config;
use crate::hc::{Client, StreamOutcome, Subscription};
use crate::{config, doorbell, state, workspace};
use doorbell_rt::{BrainMsg, Effect, FileStore, Host};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// The one live wake-socket consumer slot: the abort handle of the running
/// consumer plus its stop flag. Named so the field does not read as a tower of
/// generics (clippy::type_complexity).
type WakeTaskSlot =
    Arc<Mutex<Option<(tokio::task::AbortHandle, Arc<std::sync::atomic::AtomicBool>)>>>;

/// bd-gefp: how often the embedded host asks the plane whether its workspace
/// still resolves. The 1s liveness poll decides; this only paces the probe.
const WORKSPACE_PROBE_SECS: u64 = 30;

/// The embedded host: owns the reducer model and every flywheel-shaped
/// capability the brain orchestrates.
pub struct RtHost {
    cfg: Config,
    project: String,
    workspace: String,
    model: workspace::WorkspaceModel,
    index: doorbell::SelfIndex,
    generation: Option<doorbell::WatcherGen>,
    exe_path: PathBuf,
    brain_tx: mpsc::Sender<BrainMsg>,
    /// Panes with an active multiplexed events.subscribe forwarder.
    /// Shared with the reader thread so stream death can clear coverage and
    /// the next sweep re-arms (hc/tuios: stream death must not leave a
    /// pane stuck "covered" forever).
    covered_panes: Arc<Mutex<HashSet<String>>>,
    /// One rex presence poller per host (wave17-b): the boot seed is applied
    /// directly, and ongoing status changes ride a dedicated poll thread so
    /// no blocking plane call ever runs inside the brain's async tick.
    presence_armed: bool,
    /// Shared with the delivery path. bd-kz54 follow-up: `WatchRetry::reset()`
    /// had exactly one caller in the whole tree — its own unit test — so four
    /// failures anywhere in a host's life latched the 16s gate permanently and
    /// the transport never regained a fast first retry.
    retry: std::sync::Arc<std::sync::Mutex<doorbell_rt::WatchRetry>>,
    authority: crate::authority::RuntimeAuthority,
    index_refreshed_at: u64,
    index_pins: Vec<String>,
    /// The one live wake-socket consumer. Shared with the restart task so a
    /// replacement aborts its predecessor instead of littering watchers.
    wake_task: WakeTaskSlot,
    /// mcc9: exact wake → brain counters for this workspace, shared by every
    /// watch source this host arms (bus watch and each wake-sock consumer).
    wake_counters: Arc<doorbell_rt::wake_brain::WakeBrainCounters>,
    /// mcc9: the last line the brain accepted; a replacement consumer resumes
    /// there instead of replaying from origin.
    wake_resume: Arc<Mutex<Option<String>>>,
    /// Same bell the embedded brain selects on. A replacement consumer rings
    /// this instead of queueing `Died` behind a full watch channel.
    died_bell: std::sync::Arc<doorbell_rt::DiedBell>,
    /// bd-b2b3: the run-state recorder, `None` unless explicitly attached.
    /// The recorder is an EFFECT-layer component (it owns a store handle and a
    /// clock) and is therefore held HERE, beside the model, and never touched
    /// from a reducer. `None` means "no recorder", which is visible rather than
    /// mistaken for "no boundaries recorded yet".
    state_log: Option<crate::run_state_recorder::RunStateRecorder>,
    /// The writer thread draining `state_log`. Held so the handle is not
    /// dropped; the thread runs for the process lifetime.
    #[allow(dead_code)]
    state_log_writer: Option<std::thread::JoinHandle<()>>,
    /// T3 (bd-ma3y): services for durable interpret, built once per process
    /// (see durable_services). Borrowed, never owned: owning a
    /// ManagedRuntime here would drop a tokio Runtime inside the async
    /// Brain. One arm today (Audit); later arms share this runtime.
    durable_services: &'static ManagedRuntime,
}

/// Map one wake-sock outcome onto `BrainMsg`. Single owner for the twin
/// sites (restart + initial fallback): Watch lines and heartbeats stamp the
/// watchbeat, transport death re-enters as `Died`.
fn wake_outcome_to_brain(
    out: doorbell_rt::wake_sock::WakeOutcome,
    fw: &doorbell_rt::wake_brain::WatchForwarder,
    tx: &mpsc::Sender<BrainMsg>,
    workspace: &str,
    retry: &std::sync::Mutex<doorbell_rt::WatchRetry>,
    retired: &std::sync::atomic::AtomicBool,
    died_bell: &doorbell_rt::DiedBell,
) {
    use doorbell_rt::wake_brain::Forwarded;
    use doorbell_rt::wake_sock::WakeOutcome;
    match out {
        WakeOutcome::Watch(l) => match fw.forward(l) {
            f if f.reached_brain() => {
                doorbell_rt::watchbeat::touch_event(&config::state_dir(), workspace);
                if let Ok(mut r) = retry.lock() {
                    r.reset();
                }
            }
            Forwarded::Dropped {
                report_death: true, ..
            } => {
                retired.store(true, std::sync::atomic::Ordering::SeqCst);
                doorbell_rt::msg::deliver_died(tx, Some(died_bell));
            }
            _ => {}
        },
        WakeOutcome::Heartbeat => {
            doorbell_rt::watchbeat::touch_heartbeat(&config::state_dir(), workspace);
        }
        WakeOutcome::EventsLost { .. } => {
            fw.note_events_lost();
            crate::inbox_host::signal_invalidate(toron_core::inbox_program::Invalidate::EventsLost);
        }
        WakeOutcome::Died => {
            retired.store(true, std::sync::atomic::Ordering::SeqCst);
            doorbell_rt::msg::deliver_died(tx, Some(died_bell));
        }
    }
}

/// mcc9: one forwarder per watch source, sharing the host's counters and
/// resume cursor; sampled audit rows land in the workspace audit and the
/// snapshot under `<state>/doorbell/<ws>.wake-brain.json`.
fn wake_forwarder(
    tx: &mpsc::Sender<BrainMsg>,
    workspace: &str,
    counters: Arc<doorbell_rt::wake_brain::WakeBrainCounters>,
    resume: Arc<Mutex<Option<String>>>,
) -> doorbell_rt::wake_brain::WatchForwarder {
    let state = config::state_dir();
    let ws = workspace.to_string();
    doorbell_rt::wake_brain::WatchForwarder::new(tx.clone(), counters, resume)
        .with_audit(Arc::new(move |kind, detail| {
            doorbell::audit_append(
                &state,
                &ws,
                serde_json::json!({ "kind": kind, "detail": detail }),
            );
        }))
        .persist_to(doorbell_rt::wake_brain::snapshot_path(
            &config::state_dir(),
            workspace,
        ))
}

impl RtHost {
    pub fn new(
        cfg: Config,
        project: &str,
        workspace: &str,
        brain_tx: mpsc::Sender<BrainMsg>,
    ) -> anyhow::Result<Self> {
        let mut model = workspace::WorkspaceModel::from_environment();
        if let Some(pin) = state::load_captain_alias(project) {
            model.roles.insert("captain".into(), pin);
        }
        if let Some(topo) = state::load_session_topology(project) {
            model.session_topology = Some(topo);
        }
        crate::backend::run_mail_off_runtime_sync(|| -> anyhow::Result<()> {
            crate::durable::seed_workspace_from_active_loop(&mut model, project)?;
            crate::seed_roles_from_binds(&mut model, project)?;
            crate::cmd::doorbell::rebuild_assignment_from_receipts(&mut model);
            Ok(())
        })?;
        model.last_now = unix_now();
        let state_log = match std::env::var("FLYWHEEL_RUN_STATE_LOG") {
            Ok(v) if v.trim() == "1" || v.trim().eq_ignore_ascii_case("true") => {
                crate::run_state_recorder::RunStateRecorder::attach(&format!("flywheel/{project}"))
            }
            _ => None,
        };
        let state_log_writer = state_log
            .clone()
            .map(crate::run_state_recorder::spawn_writer);
        let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
        let index = build_index(&cfg, project);
        let now = unix_now();
        let index_pins = state::load_spawn_registry(project)
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect();
        Ok(Self {
            cfg,
            project: project.to_string(),
            workspace: workspace.to_string(),
            model,
            index,
            generation: None,
            exe_path,
            brain_tx,
            covered_panes: Arc::new(Mutex::new(HashSet::new())),
            presence_armed: false,
            retry: std::sync::Arc::new(std::sync::Mutex::new(doorbell_rt::WatchRetry::default())),
            authority: crate::authority::RuntimeAuthority::resolve(),
            index_refreshed_at: now,
            index_pins,
            wake_task: std::sync::Arc::new(std::sync::Mutex::new(None)),
            wake_counters: doorbell_rt::wake_brain::counters_for(workspace),
            wake_resume: Arc::new(Mutex::new(None)),
            died_bell: doorbell_rt::DiedBell::new(),
            state_log,
            state_log_writer,
            durable_services: durable_services(),
        })
    }

    pub fn set_generation(&mut self, generation: Option<doorbell::WatcherGen>) {
        self.generation = generation;
    }

    /// T3 arm 1 (bd-ma3y): run Audit cmds through the boot-built edge and
    /// fold their outcomes. Returns the cmds the rt effect layer still owns.
    /// Outcomes fold quiet (sentinel), so the model only gains ambient
    /// per-message bookkeeping. FactMiss keeps today's path: the final match
    /// in tick still renders it as Effect::Audit for the rt journal.
    fn run_durable_audit(&mut self, cmds: Vec<workspace::Cmd>) -> Vec<workspace::Cmd> {
        let mut rest = Vec::with_capacity(cmds.len());
        for cmd in cmds {
            if !matches!(cmd, workspace::Cmd::Audit { .. }) {
                rest.push(cmd);
                continue;
            }
            let exit = self
                .durable_services
                .run(DurableInterpret::interpret_in_env(
                    &cmd,
                    self.durable_services.context(),
                ));
            let (next, _) = match exit {
                Exit::Success(msg) => workspace::tick(&self.model, &msg),
                Exit::Failure(cause) => workspace::tick(
                    &self.model,
                    &workspace::WorkspaceProgram::failure(&Failure::of_cause(cause)),
                ),
            };
            self.model = next;
        }
        rest
    }

    /// Share the brain's death bell. Call before the first wake arms.
    pub fn set_died_bell(&mut self, bell: std::sync::Arc<doorbell_rt::DiedBell>) {
        self.died_bell = bell;
    }

    fn registry_pins(&self) -> Vec<String> {
        state::load_spawn_registry(&self.project)
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect()
    }

    /// Rex presence producer (wave17-b): the SAME source `flywheel status`
    /// trusts. Seeds one `agent_list()` (so a host that starts while panes
    /// are idle is injectable at once), then streams `rex events` scoped to
    /// the covered panes and folds every status change into a PlaneStatus.
    /// Pane ids the registry cannot map to a pin are audited loudly once —
    /// a covered pane is never silently skipped.
    fn cover_rex_panes(&mut self) {
        let registry = state::load_spawn_registry(&self.project);
        let covered = self.covered_panes.lock().unwrap_or_else(|e| e.into_inner());
        let pane_ids: Vec<(String, String)> = registry
            .iter()
            .filter(|e| !e.pane_id.is_empty())
            .filter(|e| !covered.contains(&e.pane_id))
            .filter_map(|e| e.mail_identity.clone().map(|pin| (e.pane_id.clone(), pin)))
            .collect();
        drop(covered);
        if pane_ids.is_empty() {
            return;
        }
        if self.presence_armed {
            return;
        }
        let pin_by_pane: HashMap<String, String> = pane_ids.iter().cloned().collect();
        // Seed: one bounded agent_list before the loop, so idle panes are
        // injectable without waiting for a status change.
        let backend = match crate::backend::factory::multiplexer_backend(&self.cfg) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("flywheel doorbell: rex backend unavailable ({e:#}) — no push status");
                return;
            }
        };
        // Seed: apply the statuses to the model DIRECTLY. They must not ride
        // the brain channel: it is drained only after `drive`, and a seed
        // routed through it was observed lost before the first tick (live:
        // no PlaneStatus row ever reached the fold, so every wake queued
        // forever). Direct application is deterministic.
        match backend.agent_list() {
            Ok(agents) => {
                let (updates, unmapped) = Self::presence_updates_for(&agents, &pin_by_pane);
                let live_pins: HashSet<String> = agents
                    .iter()
                    .filter_map(|a| pin_by_pane.get(&a.pane_id).cloned())
                    .collect();
                let mut applied = 0usize;
                for update in updates {
                    if let BrainMsg::PlaneStatus { pin, status } = update
                        && live_pins.contains(&pin)
                    {
                        let (next, _) = workspace::tick(
                            &self.model,
                            &workspace::Msg::PlaneStatus { pin, status },
                        );
                        self.model = next;
                        applied += 1;
                    }
                }
                if applied > 0 {
                    eprintln!(
                        "flywheel doorbell: {} seeded {applied} plane status(es)",
                        self.workspace
                    );
                }
                Self::audit_unmapped_panes(&self.workspace, &unmapped);
            }
            Err(e) => {
                eprintln!("flywheel doorbell: rex agent_list failed ({e:#}) — no seed statuses");
            }
        }
        // Ongoing: one bounded poll thread, NOT a blocking plane call inside
        // the async tick. `rex events` subscribe rejects the whole stream on
        // one dead pane id, and calling the plane from the sweep path stalled
        // the brain; a 3s agent_list poll is bounded, cheap, and reconnects
        // by construction.
        self.presence_armed = true;
        let tx = self.brain_tx.clone();
        let cfg = self.cfg.clone();
        let project = self.project.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(3));
                let Ok(backend) = crate::backend::factory::multiplexer_backend(&cfg) else {
                    continue;
                };
                let Ok(agents) = backend.agent_list() else {
                    continue;
                };
                // Re-read the registry every cycle: `flywheel add` panes are
                // only in the registry after the host started, and a map
                // frozen at boot would leave them on the old unknown path.
                let pin_by_pane: HashMap<String, String> = state::load_spawn_registry(&project)
                    .iter()
                    .filter(|e| !e.pane_id.is_empty())
                    .filter_map(|e| e.mail_identity.clone().map(|pin| (e.pane_id.clone(), pin)))
                    .collect();
                let (updates, _) = RtHost::presence_updates_for(&agents, &pin_by_pane);
                for msg in updates {
                    if tx.blocking_send(msg).is_err() {
                        return;
                    }
                }
            }
        });
    }

    /// Map backend agent rows onto PlaneStatus sends (wave17-b: the SAME
    /// source `flywheel status` trusts). Returns the messages plus any pane
    /// ids the registry could not map to a pin — the caller audits those
    /// loudly instead of silently skipping a covered pane.
    fn presence_updates_for(
        agents: &[toron_core::pane_types::AgentInfo],
        pin_by_pane: &HashMap<String, String>,
    ) -> (Vec<BrainMsg>, Vec<String>) {
        let mut updates = Vec::new();
        let mut unmapped = Vec::new();
        for ag in agents {
            match pin_by_pane.get(&ag.pane_id) {
                Some(pin) => updates.push(BrainMsg::PlaneStatus {
                    pin: pin.clone(),
                    status: ag.agent_status.as_str().to_string(),
                }),
                None => unmapped.push(ag.pane_id.clone()),
            }
        }
        updates.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        unmapped.sort();
        unmapped.dedup();
        (updates, unmapped)
    }

    /// Loud audit for panes no pin resolves (wave17-b: never silently skip
    /// a covered pane). Best-effort file append like the other host audits.
    fn audit_unmapped_panes(workspace: &str, panes: &[String]) {
        if panes.is_empty() {
            return;
        }
        let detail = serde_json::json!({
            "kind": "presence_unmapped_pane",
            "workspace": workspace,
            "panes": panes,
        });
        eprintln!(
            "flywheel doorbell: {workspace} has status for unmapped panes {panes:?} — no pin to wake; audit recorded"
        );
        doorbell::audit_append(&crate::config::state_dir(), workspace, detail);
    }

    /// Spawn push forwarders for registry panes without coverage (bd-8hv0):
    /// late crew panes get push coverage instead of the polling fallback.
    ///
    /// Hc only: when another backend is selected there is no hc terminal
    /// to subscribe, and the level-triggered ticks decide without hints
    /// (bd-g5zs). A backend-native subscribe is a follow-up, not a silent
    /// hc attempt that fails every sweep.
    ///
    /// Multiplex: one `events.subscribe` covering all uncovered crew panes
    /// (hc/tuios per-caller stream cap). On stream death, clear covered
    /// so the next sweep re-arms. If subscribe rejects because a listed pane
    /// died, refresh the pane list and retry. `events_lost` stays TODO until
    /// toron-core surfaces it as a typed signal (see hook below).
    fn cover_new_panes(&mut self) {
        if crate::pane::rex_door_selected(&self.cfg) {
            self.cover_rex_panes();
            return;
        }
        if crate::backend::resolve_backend_name(&self.cfg) != "hc" {
            return;
        }
        let registry = state::load_spawn_registry(&self.project);
        let covered = self.covered_panes.lock().unwrap_or_else(|e| e.into_inner());
        let pane_ids: Vec<(String, String)> = registry
            .iter()
            .filter(|e| !e.pane_id.is_empty())
            .filter(|e| !covered.contains(&e.pane_id))
            .filter_map(|e| e.mail_identity.clone().map(|pin| (e.pane_id.clone(), pin)))
            .collect();
        drop(covered);
        if pane_ids.is_empty() {
            return;
        }
        let tx = self.brain_tx.clone();
        let socket = self.cfg.socket_path();
        let pin_by_pane: HashMap<String, String> = pane_ids.iter().cloned().collect();
        let mut attempt_ids: Vec<String> = pane_ids.iter().map(|(p, _)| p.clone()).collect();

        let mut stream = None;
        for _round in 0..3 {
            let Ok(sub_client) = Client::connect(Some(&socket)) else {
                return;
            };
            let subs = Subscription::agent_status_changed_many(attempt_ids.iter().cloned());
            match crate::hc::subscribe(&sub_client, &subs) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => {
                    eprintln!(
                        "flywheel doorbell: multiplex subscribe rejected ({e:#}) — refreshing pane list"
                    );
                    let live: HashSet<String> = Client::connect(Some(&socket))
                        .ok()
                        .and_then(|c| c.pane_list().ok())
                        .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
                        .unwrap_or_default();
                    attempt_ids.retain(|id| live.contains(id));
                    if attempt_ids.is_empty() {
                        return;
                    }
                }
            }
        }
        let Some(mut stream) = stream else {
            return;
        };

        {
            let mut covered = self.covered_panes.lock().unwrap_or_else(|e| e.into_inner());
            covered.extend(attempt_ids.iter().cloned());
        }

        let covered_shared = Arc::clone(&self.covered_panes);
        let clear_ids = attempt_ids.clone();
        let socket_for_reconcile = socket.clone();
        std::thread::spawn(move || {
            loop {
                match stream.read_outcome() {
                    Ok(StreamOutcome::Event(ev)) => {
                        let ev = *ev;
                        if let crate::hc::TerminalEventData::PaneAgentStatusChanged(change)
                        | crate::hc::TerminalEventData::PaneAgentStatusChangedSub(change) =
                            ev.data
                        {
                            let pin = pin_by_pane
                                .get(&change.pane_id)
                                .cloned()
                                .unwrap_or_else(|| change.pane_id.clone());
                            let msg = BrainMsg::PlaneStatus {
                                pin,
                                status: change.agent_status.as_str().to_string(),
                            };
                            if tx.blocking_send(msg).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(StreamOutcome::EventsLost) => {
                        eprintln!(
                            "flywheel doorbell: events_lost — reconciling status via session.snapshot"
                        );
                        crate::inbox_host::signal_invalidate(
                            toron_core::inbox_program::Invalidate::EventsLost,
                        );
                        reconcile_status_after_events_lost(
                            &socket_for_reconcile,
                            &pin_by_pane,
                            &tx,
                        );
                    }
                    Ok(StreamOutcome::Closed) | Err(_) => {
                        clear_covered(&covered_shared, &clear_ids);
                        break;
                    }
                }
            }
        });
    }
}

fn clear_covered(covered: &Mutex<HashSet<String>>, ids: &[String]) {
    if let Ok(mut g) = covered.lock() {
        for id in ids {
            g.remove(id);
        }
    }
}

/// After `events_lost`: pull authoritative status via `session.snapshot`
/// (fall back to `agent.list`) and re-enter as PlaneStatus Msgs. Does not
/// invent delivery — only status facts for covered pins.
fn reconcile_status_after_events_lost(
    socket: &str,
    pin_by_pane: &HashMap<String, String>,
    tx: &mpsc::Sender<BrainMsg>,
) {
    let Ok(c) = Client::connect(Some(socket)) else {
        eprintln!("flywheel doorbell: events_lost reconcile — connect failed");
        return;
    };
    let agents = match c.call("session.snapshot", serde_json::json!({})) {
        Ok(snap) => agents_from_session_snapshot(&snap)
            .unwrap_or_else(|| c.agent_list().unwrap_or_default()),
        Err(e) => {
            eprintln!(
                "flywheel doorbell: session.snapshot failed ({e:#}) — falling back to agent.list"
            );
            c.agent_list().unwrap_or_default()
        }
    };
    for ag in agents {
        let Some(pin) = pin_by_pane.get(&ag.pane_id) else {
            continue;
        };
        let msg = BrainMsg::PlaneStatus {
            pin: pin.clone(),
            status: ag.agent_status.as_str().to_string(),
        };
        if tx.blocking_send(msg).is_err() {
            break;
        }
    }
}

/// Pull AgentInfo rows out of a session.snapshot payload when present.
fn agents_from_session_snapshot(snap: &serde_json::Value) -> Option<Vec<crate::hc::AgentInfo>> {
    if let Some(arr) = snap.get("agents").and_then(|v| v.as_array()) {
        let parsed: Result<Vec<_>, _> = arr.iter().cloned().map(serde_json::from_value).collect();
        return parsed.ok();
    }
    if let Some(arr) = snap.get("panes").and_then(|v| v.as_array()) {
        let parsed: Result<Vec<_>, _> = arr.iter().cloned().map(serde_json::from_value).collect();
        return parsed.ok();
    }
    None
}

fn build_index(cfg: &Config, project: &str) -> doorbell::SelfIndex {
    let pins: Vec<String> = state::load_spawn_registry(project)
        .iter()
        .filter_map(|e| e.mail_identity.clone())
        .collect();
    crate::backend::run_mail_off_runtime_sync(|| {
        crate::cmd::doorbell::doorbell_self_index(cfg, project, &pins)
    })
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Host for RtHost {
    fn tick(&mut self, msg: &BrainMsg) -> Vec<Effect> {
        let (wmsg, drain_waits) = match msg {
            BrainMsg::Watch(line) => (workspace::Msg::WatchNdjson { line: line.clone() }, true),
            BrainMsg::PlaneStatus { pin, status } => (
                workspace::Msg::PlaneStatus {
                    pin: pin.clone(),
                    status: status.clone(),
                },
                false,
            ),
            BrainMsg::Died => (workspace::Msg::WatchDied, false),
            BrainMsg::Sweep { now } => (workspace::Msg::Sweep { now: *now }, true),
            BrainMsg::RoleBound { role, pin } => (
                workspace::Msg::RoleBound {
                    role: role.clone(),
                    pin: pin.clone(),
                },
                false,
            ),
            BrainMsg::Fact { summary } => {
                if let Some(reason) = workspace::material_context_reason(summary) {
                    (workspace::material_context_msg(summary, reason), false)
                } else {
                    (
                        workspace::Msg::Fact {
                            summary: summary.clone(),
                        },
                        false,
                    )
                }
            }
            BrainMsg::AckSlaTick { now } => (workspace::Msg::AckSlaTick { now: *now }, false),
            BrainMsg::Presence { pin, state } => (
                workspace::Msg::Presence {
                    pin: pin.clone(),
                    state: state.clone(),
                },
                false,
            ),
            BrainMsg::InboxFetched { pin, rows } => (
                workspace::Msg::InboxRows {
                    rows: rows.clone(),
                    pin: pin.clone(),
                },
                false,
            ),
            BrainMsg::SlaFired { posts } => (
                workspace::Msg::SlaFired {
                    posts: posts.clone(),
                },
                false,
            ),
            BrainMsg::Degraded { .. }
            | BrainMsg::Delivered { .. }
            | BrainMsg::Ask { .. }
            | BrainMsg::Shutdown => return Vec::new(),
        };
        let (next, cmds) = workspace::tick(&self.model, &wmsg);
        self.model = next;
        let snap = crate::backend::run_mail_off_runtime_sync(|| {
            crate::live_decide_snapshot(&self.cfg, &self.project)
        });
        let cmds = workspace::expand_reclassify_cmds(
            &mut self.model,
            cmds,
            &self.project,
            &self.authority,
            &|goal| {
                let d = crate::decide::decide(goal, &snap);
                (
                    d.primitive.to_string(),
                    d.topology.to_string(),
                    d.argv,
                    d.reason,
                )
            },
        );
        if let Some(topo) = self.model.session_topology.as_deref() {
            let _ = crate::state::save_session_topology(&self.project, topo);
        }
        let cmds = self.run_durable_audit(cmds);
        crate::cmd::doorbell::handle_workspace_cmds(
            &mut self.model,
            &cmds,
            &self.project,
            &self.cfg,
            drain_waits,
        );
        if let Some(recorder) = self.state_log.as_ref()
            && let Err(refusal) = recorder.enqueue(&self.model, &[])
        {
            eprintln!("flywheel: run-state boundary not recorded: {refusal}");
        }
        cmds.into_iter()
            .filter_map(|c| match c {
                workspace::Cmd::Prompt { pin, line } => Some(Effect::Prompt { pin, line }),
                workspace::Cmd::FetchInbox => Some(Effect::FetchInbox),
                workspace::Cmd::CheckAckSla => Some(Effect::CheckAckSla),
                workspace::Cmd::RestartWatch => Some(Effect::RestartWatch),
                workspace::Cmd::FactMiss { why } => Some(Effect::Audit {
                    kind: "fact_miss".into(),
                    pin: String::new(),
                    detail: serde_json::json!({ "why": why }),
                }),
                workspace::Cmd::Audit { kind, pin, detail } => Some(Effect::Audit {
                    kind,
                    pin,
                    detail: serde_json::json!(detail),
                }),
                _ => None,
            })
            .collect()
    }

    fn now_unix(&self) -> u64 {
        unix_now()
    }

    /// GS-039 transition gate (audit P1): the reducer model owns the
    /// last-known status — `tick` ran `workspace::tick` before the brain
    /// reads this, so it must return the PREVIOUS tick's value. Untrained
    /// pins (not in the map) report None (unknown → transitioned).
    fn prev_status(&self, pin: &str) -> Option<String> {
        self.model.doorbell.statuses.get(pin).cloned()
    }

    /// Fetch-only slice of the old sweep arm (classify + prompt moved to
    /// tick via InboxRows — the bd-9hco pipe, unchanged semantics).
    /// F6: shared borrow (no mutation; enables future off-loop pipe).
    fn fetch_inbox(&self, pin: &str, since: &str) -> Vec<serde_json::Value> {
        if let Some(rows) = self.fetch_inbox_process(pin, since) {
            return rows;
        }
        self.fetch_inbox_cli(pin, since)
    }

    fn scan_ack_sla(&mut self) -> Vec<String> {
        let Some(mb) = crate::mail::try_mailbox() else {
            return Vec::new();
        };
        match crate::backend::run_mail_off_runtime_sync(|| {
            crate::mail::block_on(mb.check_ack_sla_expired(&self.project, None))
        }) {
            Ok(emitted) if emitted.is_empty() => Vec::new(),
            Ok(emitted) => {
                doorbell::audit_append(
                    &config::state_dir(),
                    &self.workspace,
                    serde_json::json!({
                        "ts": chrono::Utc::now().to_rfc3339(),
                        "workspace": self.workspace,
                        "kind": "ack_sla_fired",
                        "count": emitted.len(),
                        "posts": emitted.iter().take(10).collect::<Vec<_>>(),
                    }),
                );
                emitted
            }
            Err(e) => {
                eprintln!("flywheel doorbell: ack-sla scan failed: {e:#}");
                Vec::new()
            }
        }
    }

    fn inject_allowed(&self, _pin: &str) -> bool {
        match &self.generation {
            Some(record) => doorbell::inject_allowed(record, &self.exe_path),
            None => true,
        }
    }

    fn pins(&self) -> Vec<String> {
        let registry = state::load_spawn_registry(&self.project);
        let unpinned: Vec<String> = registry
            .iter()
            .filter(|e| e.mail_identity.as_deref().is_none_or(str::is_empty))
            .map(|e| e.pane_id.clone())
            .collect();
        if !unpinned.is_empty() {
            eprintln!(
                "flywheel: warn: doorbell sweep skipping {} unpinned pane(s) with no mail identity ({}); re-register with a mail identity to restore inbox checks",
                unpinned.len(),
                unpinned.join(", ")
            );
        }
        registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect()
    }

    fn sweep_maintenance(&mut self) -> Vec<(String, String)> {
        let pins_now: Vec<String> = self.registry_pins();
        let now = unix_now();
        if pins_now != self.index_pins || now.saturating_sub(self.index_refreshed_at) >= 60 {
            self.index = build_index(&self.cfg, &self.project);
            self.index_pins = pins_now;
            self.index_refreshed_at = now;
        }
        self.cover_new_panes();
        let path = crate::mail::presence_path();
        let Ok(body) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let pins: Vec<String> = self.registry_pins();
        crate::cmd::doorbell::timeout_presence_rows(&body, unix_now())
            .into_iter()
            .filter(|(pin, _)| pins.contains(pin))
            .collect()
    }

    fn drain_role_bounds(&mut self) -> Vec<(String, String)> {
        crate::drain_pending_role_bounds(&self.project).unwrap_or_else(|e| {
            eprintln!(
                "flywheel doorbell: drain_pending_role_bounds failed (refusing silent skip): {e:#}"
            );
            Vec::new()
        })
    }

    fn drain_facts(&mut self) -> Vec<String> {
        crate::drain_pending_facts(&crate::config::state_dir(), &self.project)
    }

    /// Always `true`: this host has no single-flight gate (it spawns a task per
    /// call and spaces them with the retry backoff), so every death really does
    /// schedule its own restart, and the journal row the brain writes for it is
    /// honest about that. The counts on the daemon path come from `FileHost`,
    /// which does coalesce (bd-watch-restart-metric-8dvf).
    fn restart_wake(&mut self) -> bool {
        let backoff = self.retry.lock().map(|mut r| r.record_fail()).unwrap_or(16);
        let tx = self.brain_tx.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let slot = self.wake_task.clone();
        let retry = self.retry.clone();
        let counters = self.wake_counters.clone();
        let resume = self.wake_resume.clone();
        let died_bell = std::sync::Arc::clone(&self.died_bell);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            if let Ok(mut g) = slot.lock()
                && let Some((h, retire)) = g.take()
            {
                retire.store(true, std::sync::atomic::Ordering::SeqCst);
                h.abort();
            }
            let tx_g = tx.clone();
            let beat_workspace = workspace.clone();
            let retry = retry.clone();
            let resume_from = resume.lock().ok().and_then(|g| g.clone());
            if let Some(from) = &resume_from {
                counters.note_resume();
                doorbell::audit_append(
                    &config::state_dir(),
                    &workspace,
                    serde_json::json!({
                        "kind": doorbell_rt::wake_brain::AUDIT_WAKE_RESUMED,
                        "detail": { "resume_from": from },
                    }),
                );
            }
            let fw = wake_forwarder(&tx_g, &workspace, counters.clone(), resume.clone());
            let retired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let retired_cb = std::sync::Arc::clone(&retired);
            let wake_bell = std::sync::Arc::clone(&died_bell);
            let handle = crate::cmd::doorbell::spawn_wake_consumer(
                &project,
                &workspace,
                &config::state_dir(),
                resume_from,
                retired,
                move |out| {
                    wake_outcome_to_brain(
                        out,
                        &fw,
                        &tx_g,
                        &beat_workspace,
                        &retry,
                        &retired_cb,
                        &wake_bell,
                    );
                },
            );
            if let Ok(mut g) = slot.lock() {
                *g = Some(handle);
            }
            doorbell::audit_append(
                &config::state_dir(),
                "",
                serde_json::json!({ "kind": "watch_restart" }),
            );
        });
        true
    }
}

impl RtHost {
    fn fetch_inbox_process(&self, pin: &str, since: &str) -> Option<Vec<serde_json::Value>> {
        crate::backend::run_mail_off_runtime_sync(|| {
            let mail = crate::mail::Mail::from_config(Some(&self.cfg.toron_mail_config()))
                .ok()
                .flatten()?;
            let filter = crate::mail::MailInboxFilter {
                since_ts: Some(since.to_string()),
                limit: 50,
                include_bodies: true,
                ..Default::default()
            };
            let v = mail.inbox_filtered(&self.project, pin, &filter).ok()?;
            v.as_array().cloned()
        })
    }

    fn fetch_inbox_cli(&self, pin: &str, since: &str) -> Vec<serde_json::Value> {
        let output = std::process::Command::new("toron")
            .args(crate::cmd::internal::mail_inbox_fallback_argv(
                pin,
                &self.project,
                since,
            ))
            .output();
        let Ok(output) = output else {
            doorbell::audit_append(
                &config::state_dir(),
                &self.workspace,
                serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "workspace": self.workspace,
                    "kind": "inbox_error",
                    "pin": pin,
                    "error": "toron spawn failed",
                }),
            );
            return Vec::new();
        };
        if !output.status.success() {
            doorbell::audit_append(
                &config::state_dir(),
                &self.workspace,
                serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "workspace": self.workspace,
                    "kind": "inbox_error",
                    "pin": pin,
                    "error": String::from_utf8_lossy(&output.stderr).to_string(),
                }),
            );
            return Vec::new();
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
        match &parsed {
            serde_json::Value::Array(arr) => arr.clone(),
            v if v.get("messages").is_some() => v
                .get("messages")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default(),
            other if other.is_object() => vec![other.clone()],
            _ => vec![],
        }
    }
}

pub fn run_embedded(cfg: &Config, project: &str, workspace: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let now = doorbell_rt::lease::unix_now();
    let lease = match doorbell_rt::lease::acquire_lease(&config::state_dir(), workspace, now) {
        Ok(l) => l,
        Err(doorbell_rt::lease::LeaseDenied::Held(h))
            if crate::comms_send::enabled(cfg) && !doorbell_rt::lease::is_ringer(&h) =>
        {
            return Err(anyhow::anyhow!(
                "flywheel doorbell: no-text host holds {project}/{workspace} but serve never hosts and the inbox host cannot ring NIP-17 (head=0) — clear the leftover lease for {workspace} and retry; refusing to silently host nothing"
            ));
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "flywheel doorbell: embedded refuse double-run (live lease): {e:?}"
            ));
        }
    };
    let path = doorbell::pid_file(&config::state_dir(), workspace);
    doorbell::write_pid(&path, std::process::id())?;
    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
    let gen_path = doorbell::gen_file(&config::state_dir(), workspace);
    // Always capture THIS host's binary. Reusing a gen left by a pre-install
    // spawn made the liveness check see a stale binary and exit-loop under
    // launchd (the host itself is the current binary).
    let generation = doorbell::WatcherGen::capture(&exe_path, unix_now())
        .filter(|record| doorbell::write_gen(&gen_path, record).is_ok());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("doorbell rt runtime")?;
    let res = rt.block_on(run_embedded_async(
        cfg.clone(),
        project,
        workspace,
        generation,
    ));
    doorbell_rt::lease::release_lease(&config::state_dir(), workspace, &lease);
    res
}

async fn run_embedded_async(
    cfg: Config,
    project: &str,
    workspace: &str,
    generation: Option<doorbell::WatcherGen>,
) -> anyhow::Result<()> {
    use doorbell_rt::SinkManager;
    // wave7-a2: the loop (channel, tickers, beads, brain drive) is the one
    // runner; this entry point only configures in-process transport/sinks.
    let mut runner = doorbell_rt::HostRunner::start(doorbell_rt::RunnerConfig::standard(workspace));
    let tx = runner.sender();
    let died_bell = runner.died_bell();
    let mut host = RtHost::new(cfg.clone(), project, workspace, tx.clone())?;
    host.set_died_bell(std::sync::Arc::clone(&died_bell));
    host.set_generation(generation.clone());
    let mut sinks = SinkManager::new(tx.clone());
    let mut _inbox_guard: Option<crate::inbox_host::InboxHost> = None;
    // The embedded host exists to TYPE. NoTextSink was the honest answer
    // while the plane could not inject; on rex it silently dropped every
    // wake (`injects=0 sheds=N`). The sink now types through the one
    // selected plane, resolving pin -> pane through the spawn registry.
    let sink_cfg = cfg.clone();
    let sink_project = project.to_string();
    sinks.add_sink_with_factory(
        "hc",
        Box::new(PlanePromptSink::new(cfg.clone(), project)),
        move || Box::new(PlanePromptSink::new(sink_cfg.clone(), &sink_project)),
    );
    if crate::comms_send::enabled(&cfg) {
        _inbox_guard = Some(crate::inbox_host::InboxHost::spawn(
            crate::inbox_ports::FlywheelPorts::new(&cfg, project, workspace),
            crate::inbox_host::HostCfg::try_from_env()?,
        ));
    }
    let store = FileStore::new(config::state_dir());
    let watch_slot = host.wake_task.clone();
    let bus_fw = wake_forwarder(
        &tx,
        workspace,
        host.wake_counters.clone(),
        host.wake_resume.clone(),
    );
    let bus_armed = {
        let (cfg_b, project_b, workspace_b, tx_b) = (
            cfg.clone(),
            project.to_string(),
            workspace.to_string(),
            tx.clone(),
        );
        let bus_bell = std::sync::Arc::clone(&died_bell);
        let inbox_hint = _inbox_guard.as_ref().map(|h| h.hint_sender());
        crate::backend::run_mail_off_runtime(move || {
            Ok(spawn_bus_watch(
                &cfg_b,
                &project_b,
                &workspace_b,
                tx_b,
                bus_fw,
                bus_bell,
                inbox_hint,
            ))
        })
        .await?
    };
    if !bus_armed {
        let tx_w = tx.clone();
        let project = project.to_string();
        let beat_workspace = workspace.to_string();
        let retry_w = host.retry.clone();
        let fw = wake_forwarder(
            &tx,
            workspace,
            host.wake_counters.clone(),
            host.wake_resume.clone(),
        );
        let retired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let retired_cb = std::sync::Arc::clone(&retired);
        let wake_bell = std::sync::Arc::clone(&died_bell);
        let handle = crate::cmd::doorbell::spawn_wake_consumer(
            &project,
            workspace,
            &config::state_dir(),
            None,
            retired,
            move |out| {
                wake_outcome_to_brain(
                    out,
                    &fw,
                    &tx_w,
                    &beat_workspace,
                    &retry_w,
                    &retired_cb,
                    &wake_bell,
                );
            },
        );
        if let Ok(mut g) = watch_slot.lock() {
            *g = Some(handle);
        }
    }
    host.cover_new_panes();
    if cfg.doorbell_beads_poller() {
        let repo = crate::project_dir(&cfg, project);
        runner.spawn_beads_poller(repo);
    }
    let beat_ws = workspace.to_string();
    let _lease_beat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            let sd = config::state_dir();
            let now = doorbell_rt::lease::unix_now();
            if let Some(mut cur) = doorbell_rt::lease::read_lease(&sd, &beat_ws) {
                doorbell_rt::lease::refresh_lease(&sd, &beat_ws, &mut cur, now);
            }
        }
    });
    // (sweep + ack-sla tickers and the beads poller live in the runner now.)
    if crate::timer_watch::host_enabled(&cfg) {
        crate::timer_watch::spawn_sweep(project);
    }
    let _external_rering = (cfg.doorbell_cross_workspace()
        && !cfg.doorbell_external_projects().is_empty())
    .then(|| {
        let repo = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let foreign: Vec<(String, PathBuf)> = cfg
            .doorbell_external_projects()
            .into_iter()
            .map(|(k, v)| (k, PathBuf::from(v)))
            .collect();
        doorbell_rt::pollers::spawn_external_rering(tx.clone(), repo, foreign)
    });
    let pid_path = doorbell::pid_file(&config::state_dir(), workspace);
    let gen_path = doorbell::gen_file(&config::state_dir(), workspace);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
    let probe_cfg = cfg.clone();
    let probe_project = project.to_string();
    let probe_ws = workspace.to_string();
    let probe_dir = config::state_dir();
    let shutdown_tx = tx.clone();
    // wave15-b: only a stale binary restarts the host. The flag rides out
    // of the liveness task; `run_embedded_async` turns it into a
    // restartable (nonzero) exit instead of the silent exit 0 that used to
    // strand supervised hosts on the old binary. Deliberate retires (pid
    // gone, workspace gone, daemon stop) stay exit 0 and are never
    // restarted.
    let stale_binary = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stale_binary_cb = std::sync::Arc::clone(&stale_binary);
    let _liveness = tokio::spawn(async move {
        let mut gone_streak = 0u32;
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !pid_path.exists() {
                break;
            }
            if let Some(record) = doorbell::read_gen(&gen_path)
                && !doorbell::inject_allowed(&record, &exe)
            {
                eprintln!(
                    "flywheel doorbell: binary changed since watcher start; retiring for supervised restart"
                );
                stale_binary_cb.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            ticks += 1;
            if ticks.is_multiple_of(WORKSPACE_PROBE_SECS) {
                let (cfg_p, project_p, ws_p) =
                    (probe_cfg.clone(), probe_project.clone(), probe_ws.clone());
                let mut presence = tokio::task::spawn_blocking(move || {
                    doorbell::probe_workspace_presence(&cfg_p, &project_p, &ws_p)
                })
                .await
                .unwrap_or(doorbell::WorkspacePresence::Unreadable);
                if presence == doorbell::WorkspacePresence::Gone {
                    for _ in 0..2 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let (cfg_q, project_q, ws_q) =
                            (probe_cfg.clone(), probe_project.clone(), probe_ws.clone());
                        let relist = tokio::task::spawn_blocking(move || {
                            doorbell::probe_workspace_presence(&cfg_q, &project_q, &ws_q)
                        })
                        .await
                        .unwrap_or(doorbell::WorkspacePresence::Unreadable);
                        if relist != doorbell::WorkspacePresence::Gone {
                            presence = relist;
                            break;
                        }
                    }
                }
                if let Some(reason) =
                    doorbell::host_exit_check(&probe_dir, &probe_ws, &mut gone_streak, presence)
                {
                    eprintln!(
                        "flywheel doorbell: workspace {probe_ws} no longer resolves on the plane; retiring ({reason})"
                    );
                    break;
                }
            }
        }
        let _ = shutdown_tx.send(BrainMsg::Shutdown).await;
    });
    // wave7-a2: the one runner drives the brain; producers above only send.
    let report = runner
        .drive(host, sinks, store, std::future::pending())
        .await;
    if stale_binary.load(std::sync::atomic::Ordering::SeqCst) {
        return stale_binary_exit();
    }
    _lease_beat.abort();
    if let Ok(mut g) = watch_slot.lock() {
        let _ = g.take();
    }
    eprintln!(
        "flywheel doorbell: embedded loop exited (injects={} sheds={} degraded={})",
        report.stats.injects, report.stats.sheds, report.stats.degraded
    );
    Ok(())
}

/// Restartable exit for a stale binary (wave15-b): the supervisor only
/// restarts on a nonzero exit, so a replaced binary must NOT retire 0
/// (that strands the host on dead code). Deliberate retires — pid file
/// gone, workspace gone, daemon stop — return `Ok` and stay dead.
fn stale_binary_exit() -> anyhow::Result<()> {
    anyhow::bail!(
        "flywheel doorbell: replaced binary detected — exiting nonzero so the \
         supervisor restarts this host on the new binary (re-run the command \
         to retire quietly instead of supervising it)"
    )
}

/// In-process bus watch (zero fork): kinds mirror the CLI fallback filter.
fn spawn_bus_watch(
    cfg: &Config,
    project: &str,
    workspace: &str,
    tx: mpsc::Sender<BrainMsg>,
    fw: doorbell_rt::wake_brain::WatchForwarder,
    died_bell: std::sync::Arc<doorbell_rt::DiedBell>,
    inbox_hint: Option<std::sync::mpsc::SyncSender<crate::inbox_host::Input>>,
) -> bool {
    (|| -> Option<()> {
        let _mail = crate::mail::Mail::from_config(Some(&cfg.toron_mail_config()))
            .ok()
            .flatten()?;
        let mb = crate::mail::try_mailbox()?;
        let bus = crate::mail::block_on(async {
            Ok::<std::sync::Arc<dyn toron_core::bus::Bus>, anyhow::Error>(mb.bus().clone())
        })
        .ok()?;
        let filter = {
            use toron_core::bus::Filter;
            use toron_core::nostr::kinds::toron_kind;
            Filter::new()
                .kinds([
                    toron_kind(9),
                    toron_kind(14),
                    toron_kind(20001),
                    toron_kind(30013),
                    toron_kind(43001),
                    toron_kind(30031),
                    toron_kind(39002),
                ])
                .hashtag(project)
        };
        let mut sub = crate::mail::block_on(bus.subscribe_trusted(filter)).ok()?;
        let mut stamps =
            doorbell_rt::watchbeat::spawn_stamp_threads(config::state_dir(), workspace.to_string());
        std::thread::spawn(move || {
            loop {
                let ev_opt = crate::mail::block_on(sub.next());
                match ev_opt {
                    Some(toron_core::bus::SubscriptionEvent::Event(e)) => {
                        if let Some(h) = &inbox_hint {
                            let _ = h.try_send(crate::inbox_host::Input::Hint(String::new()));
                        }
                        let tags: Vec<Vec<String>> =
                            e.tags.iter().map(|t| t.clone().to_vec()).collect();
                        let v = serde_json::json!({
                            "kind": e.kind.as_u16(),
                            "id": e.id.to_hex(),
                            "pubkey": e.pubkey.to_hex(),
                            "content": e.content,
                            "tags": tags,
                            "created_at": e.created_at.as_secs(),
                        });
                        if let Ok(line) = serde_json::to_string(&v) {
                            if fw.forward(line).reached_brain() {
                                stamps.on_event();
                            } else {
                                stamps.stop();
                                doorbell_rt::msg::deliver_died(&tx, Some(&died_bell));
                                break;
                            }
                        }
                    }
                    Some(toron_core::bus::SubscriptionEvent::Eose) => continue,
                    Some(toron_core::bus::SubscriptionEvent::Closed(_)) | None => {
                        stamps.stop();
                        doorbell_rt::msg::deliver_died(&tx, Some(&died_bell));
                        break;
                    }
                }
            }
        });
        Some(())
    })()
    .is_some()
}

#[cfg(test)]
mod stale_retire_tests {
    //! wave15-b: a replaced binary must exit nonzero (supervisor restarts
    //! it on the new code); deliberate retires stay silent exit 0.

    #[test]
    fn stale_binary_retire_is_a_restartable_error() {
        let err = super::stale_binary_exit().expect_err("stale binary must not exit 0");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("supervisor restarts"),
            "the error must name the restart contract: {msg}"
        );
    }
}

#[cfg(test)]
mod parity_tests {
    //! bd-ene2 Phase 3 parity gate: the same scripted [`BrainMsg`]log
    //! through `Brain` + embedded `RtHost` vs `Brain` + daemon `FileHost`
    //! yields byte-identical journals modulo `ts` (the bead verify).
    //!
    //! Parity surface (scripted below; divergences outside it are
    //! documented, not silent): idle/working statuses, heard presence,
    //! addressed channel/job/request-release rings, urgent breakthrough,
    //! queue/drain, role binds (classification-neutral), facts, inbox
    //! rows, sweep/ack beats. Excluded: multi-urgent replays (workspace
    //! presented ledger), ack-SLA publish (G1, no toron CLI verb),
    //! Receipt/Publish/Invoke inputs.
    use super::*;
    use doorbell_rt::{
        Brain, BrainConfig, BrainMsg, FileHost, FileHostConfig, MockSink, SinkManager,
    };

    fn strip_ts(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.remove("ts");
                for v in m.values_mut() {
                    strip_ts(v);
                }
            }
            serde_json::Value::Array(a) => {
                for v in a.iter_mut() {
                    strip_ts(v);
                }
            }
            _ => {}
        }
    }

    fn read_journal(dir: &std::path::Path, ws: &str) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(
            dir.join("journals")
                .join(format!("doorbell-journal-{ws}.jsonl")),
        )
        .unwrap_or_default();
        raw.lines()
            .filter_map(|l| {
                let mut v: serde_json::Value = serde_json::from_str(l).ok()?;
                strip_ts(&mut v);
                Some(v)
            })
            .collect()
    }

    fn watch(kind: u16, tags: serde_json::Value, id: &str, ts: u64) -> String {
        serde_json::json!({
            "kind": kind, "id": id, "pubkey": "ab",
            "tags": tags, "content": "hello", "created_at": ts
        })
        .to_string()
    }

    #[test]
    fn registry_key_mirrors_state_key() {
        for proj in ["proj", "/tmp/x/proj", "a/b/c"] {
            assert_eq!(
                doorbell_rt::registry_key(proj),
                crate::state::state_key(proj),
                "key mirror for {proj:?}"
            );
        }
    }
    #[test]
    fn embedded_vs_daemon_journal_parity() {
        let _home = crate::config::test_state_home();
        let toron_dir = std::env::temp_dir().join(format!("parity-toron-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&toron_dir);
        std::fs::create_dir_all(&toron_dir).unwrap();
        // SAFETY: under ENV_LOCK via _home; restored below.
        let prev_toron_data = std::env::var_os("TORON_DATA_DIR");
        let prev_mcp_data_dir = std::env::var_os("TORON_MCP_DATA_DIR");
        unsafe {
            std::env::set_var("TORON_DATA_DIR", &toron_dir);
            std::env::remove_var("TORON_MCP_DATA_DIR");
        }
        let proj_dir = std::env::temp_dir().join(format!("parity-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&proj_dir);
        std::fs::create_dir_all(&proj_dir).unwrap();
        let project = proj_dir.to_string_lossy().into_owned();
        let ws = "pw";
        let key = crate::state::spawn_registry_key(&project);
        let role_key = crate::state::state_key(&project);
        let now = unix_now();
        std::fs::write(
            toron_dir.join("presence.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"name": "PinA", "state": "idle", "ts": now}),
                serde_json::json!({"name": "PinB", "state": "idle", "ts": now}),
            ),
        )
        .unwrap();

        fn write_fixture(state: &std::path::Path, key: &str, role_key: &str) {
            let agents = state.join("agents");
            std::fs::create_dir_all(&agents).unwrap();
            std::fs::write(
                agents.join(format!("{key}.json")),
                serde_json::json!([
                    {"pane_id": "pane-a", "name": "hc-a", "mail_identity": "PinA"},
                    {"pane_id": "pane-b", "name": "hc-b", "mail_identity": "PinB"}
                ])
                .to_string(),
            )
            .unwrap();
            let roles = state.join("roles");
            let facts = state.join("facts");
            std::fs::create_dir_all(&roles).unwrap();
            std::fs::create_dir_all(&facts).unwrap();
            std::fs::write(
                roles.join(format!("{role_key}-pending.jsonl")),
                "{\"role\":\"coder\",\"pin\":\"PinA\"}\n",
            )
            .unwrap();
            std::fs::write(
                facts.join(format!("{role_key}-pending.jsonl")),
                "\"deploy\"\n",
            )
            .unwrap();
        }

        // SAFETY: under ENV_LOCK; the guard restores nothing — XDG is
        fn set_xdg(state: &std::path::Path) {
            unsafe {
                std::env::set_var("XDG_STATE_HOME", state);
            }
        }

        let inbox_row =
            |ts: &str| serde_json::json!({"importance": "urgent", "created_ts": ts, "tags": []});
        let mk_script = || {
            vec![
                BrainMsg::PlaneStatus {
                    pin: "PinA".into(),
                    status: "idle".into(),
                },
                BrainMsg::PlaneStatus {
                    pin: "PinB".into(),
                    status: "working".into(),
                },
                BrainMsg::Presence {
                    pin: "PinA".into(),
                    state: "idle".into(),
                },
                BrainMsg::Presence {
                    pin: "PinB".into(),
                    state: "idle".into(),
                },
                BrainMsg::Watch(watch(9, serde_json::json!([["p", "PinA"]]), "e1", 1)),
                BrainMsg::Watch(watch(9, serde_json::json!([["p", "PinB"]]), "e2", 2)),
                BrainMsg::Watch(watch(
                    43001,
                    serde_json::json!([["assignee", "PinA"]]),
                    "e3",
                    3,
                )),
                BrainMsg::PlaneStatus {
                    pin: "PinB".into(),
                    status: "idle".into(),
                },
                BrainMsg::InboxFetched {
                    pin: "PinA".into(),
                    rows: vec![inbox_row("2026-09-10T08:00:01Z")],
                },
                BrainMsg::RoleBound {
                    role: "coder".into(),
                    pin: "PinA".into(),
                },
                BrainMsg::Fact {
                    summary: "deploy".into(),
                },
                BrainMsg::Sweep { now: 1_700_000_000 },
                BrainMsg::Presence {
                    pin: "PinA".into(),
                    state: "working".into(),
                },
                BrainMsg::PlaneStatus {
                    pin: "PinA".into(),
                    status: "working".into(),
                },
                BrainMsg::Watch(watch(9, serde_json::json!([["p", "PinA"]]), "e4", 4)),
                BrainMsg::Watch(watch(
                    14,
                    serde_json::json!([["p", "PinA"], ["toron-kind", "request-release"]]),
                    "e5",
                    5,
                )),
                BrainMsg::PlaneStatus {
                    pin: "PinA".into(),
                    status: "idle".into(),
                },
            ]
        };

        async fn run_script<H: doorbell_rt::Host + Send + 'static>(
            brain: Brain<H>,
            tx: tokio::sync::mpsc::Sender<BrainMsg>,
            rx: tokio::sync::mpsc::Receiver<BrainMsg>,
            script: Vec<BrainMsg>,
        ) -> doorbell_rt::RunReport {
            let handle = tokio::spawn(async move { brain.run(rx).await });
            for msg in script {
                tx.send(msg).await.unwrap();
            }
            tx.send(BrainMsg::Shutdown).await.unwrap();
            drop(tx);
            handle.await.unwrap()
        }

        fn remote_cfg() -> crate::config::Config {
            crate::config::Config {
                mail: Some(crate::config::MailSections {
                    toron: Some(crate::config::MailServerConfig {
                        url: Some("http://127.0.0.1:9/mcp".into()),
                        token: None,
                    }),
                }),
                ..Default::default()
            }
        }

        let state_a = std::env::temp_dir().join(format!("parity-state-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_a);
        set_xdg(&state_a);
        write_fixture(&state_a, &key, &role_key);
        let dir_a = std::env::temp_dir().join(format!("parity-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_a);
        let (tx_a, rx_a) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        let rt_host = RtHost::new(remote_cfg(), &project, ws, tx_a.clone())
            .expect("rthost builds in sandbox");
        {
            use doorbell_rt::Host;
            let mut rt =
                RtHost::new(remote_cfg(), &project, ws, tx_a.clone()).expect("throwaway rthost");
            let mut fh = FileHost::new(FileHostConfig::new(
                crate::config::state_dir(),
                project.clone(),
                ws.to_string(),
            ));
            let a = rt.tick(&BrainMsg::AckSlaTick { now: 1_700_000_000 });
            let b = fh.tick(&BrainMsg::AckSlaTick { now: 1_700_000_000 });
            assert_eq!(format!("{a:?}"), format!("{b:?}"), "acksla tick parity");
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let rep_a = rt.block_on(async {
            let mut sinks_a = SinkManager::new(tx_a.clone());
            sinks_a.add_sink("hc", Box::new(MockSink::new().0));
            let brain_a = Brain::new(
                BrainConfig::new(ws),
                rt_host,
                doorbell_rt::FileStore::new(&dir_a),
                sinks_a,
            );
            run_script(brain_a, tx_a, rx_a, mk_script()).await
        });
        drop(rt);

        let state_b = std::env::temp_dir().join(format!("parity-state-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_b);
        set_xdg(&state_b);
        write_fixture(&state_b, &key, &role_key);
        let dir_b = std::env::temp_dir().join(format!("parity-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_b);
        let (tx_b, rx_b) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        let mut fcfg = FileHostConfig::new(crate::config::state_dir(), project, ws.to_string());
        fcfg.toron_bin = "true".into();
        fcfg.plane_bin = "true".into();
        let file_host = FileHost::new(fcfg).with_tx(tx_b.clone());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let rep_b = rt.block_on(async {
            let mut sinks_b = SinkManager::new(tx_b.clone());
            sinks_b.add_sink("hc", Box::new(MockSink::new().0));
            let brain_b = Brain::new(
                BrainConfig::new(ws),
                file_host,
                doorbell_rt::FileStore::new(&dir_b),
                sinks_b,
            );
            run_script(brain_b, tx_b, rx_b, mk_script()).await
        });
        drop(rt);

        assert_eq!(rep_a.stats.injects, rep_b.stats.injects, "injects");
        assert_eq!(rep_a.stats.sheds, rep_b.stats.sheds, "sheds");
        assert_eq!(rep_a.stats.degraded, rep_b.stats.degraded, "degraded");
        assert_eq!(
            rep_a.stats.digest_batches, rep_b.stats.digest_batches,
            "digests"
        );
        let a = read_journal(&dir_a, ws);
        let b = read_journal(&dir_b, ws);
        assert_eq!(a.len(), b.len(), "journal row counts differ");
        for (i, (ra, rb)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(ra, rb, "journal row {i} differs:\n{ra:?}\n{rb:?}");
        }
        unsafe {
            match prev_toron_data {
                Some(v) => std::env::set_var("TORON_DATA_DIR", v),
                None => std::env::remove_var("TORON_DATA_DIR"),
            }
            match prev_mcp_data_dir {
                Some(v) => std::env::set_var("TORON_MCP_DATA_DIR", v),
                None => std::env::remove_var("TORON_MCP_DATA_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
        let _ = std::fs::remove_dir_all(&state_a);
        let _ = std::fs::remove_dir_all(&state_b);
        let _ = std::fs::remove_dir_all(&toron_dir);
        let _ = std::fs::remove_dir_all(&proj_dir);
    }

    #[test]
    fn died_tick_parity() {
        let mut core = doorbell_rt::CoreReducer::default();
        let fx = core.tick_effects(&BrainMsg::Died);
        assert!(
            fx.iter()
                .any(|e| matches!(e, doorbell_rt::Effect::RestartWatch))
        );
    }
}

#[cfg(test)]
mod presence_producer_tests {
    //! wave17-b: PlantStatus comes from the SAME rows `flywheel status`
    //! reads. Fake agent rows (no backend) pin the mapping: an idle pin
    //! emits an injectable PlaneStatus, a working pin emits working
    //! (queued, never dropped), and an unmapped pane is reported for
    //! loud audit instead of silently skipped.
    use super::*;
    use toron_core::pane_types::{AgentInfo, AgentStatus};

    fn row(pane: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            agent: Some("agent".into()),
            agent_status: status,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: Some("agent".into()),
            pane_id: pane.into(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: String::new(),
            terminal_id: String::new(),
            terminal_title: None,
            workspace_id: "ws".into(),
        }
    }

    fn pin_map() -> HashMap<String, String> {
        HashMap::from([
            ("pane-idle".to_string(), "PinA".to_string()),
            ("pane-working".to_string(), "PinB".to_string()),
        ])
    }

    #[test]
    fn idle_maps_to_an_injectable_plane_status() {
        let agents = vec![row("pane-idle", AgentStatus::Idle)];
        let (updates, unmapped) = RtHost::presence_updates_for(&agents, &pin_map());
        assert!(unmapped.is_empty(), "mapped pins never audit: {unmapped:?}");
        assert_eq!(updates.len(), 1, "one row in, one PlaneStatus out");
        match &updates[0] {
            BrainMsg::PlaneStatus { pin, status } => {
                assert_eq!(pin, "PinA");
                assert_eq!(status, "idle");
                assert!(
                    doorbell_core::should_inject(status),
                    "idle must be injectable or the wake queues forever: {status}"
                );
            }
            other => panic!("expected PlaneStatus, got {other:?}"),
        }
    }

    #[test]
    fn working_maps_to_a_queued_not_dropped_status() {
        let agents = vec![row("pane-working", AgentStatus::Working)];
        let (updates, unmapped) = RtHost::presence_updates_for(&agents, &pin_map());
        assert!(unmapped.is_empty());
        match &updates[0] {
            BrainMsg::PlaneStatus { pin, status } => {
                assert_eq!(pin, "PinB");
                assert_eq!(status, "working");
                assert!(
                    !doorbell_core::should_inject(status),
                    "working queues; it must not read as idle: {status}"
                );
            }
            other => panic!("expected PlaneStatus, got {other:?}"),
        }
    }

    #[test]
    fn an_unmapped_pane_is_reported_never_silently_skipped() {
        let agents = vec![
            row("pane-idle", AgentStatus::Idle),
            row("pane-ghost", AgentStatus::Idle),
        ];
        let (updates, unmapped) = RtHost::presence_updates_for(&agents, &pin_map());
        assert_eq!(updates.len(), 1, "only the mapped pin emits");
        assert_eq!(unmapped, vec!["pane-ghost".to_string()]);
    }
}

#[cfg(test)]
mod events_lost_tests {
    //! toron#17: an `events_lost` outcome reaches the host's exact counters
    //! (no silent loss) and is never a death or a delivery.
    use super::*;
    use doorbell_rt::wake_sock::WakeOutcome;

    #[test]
    fn an_events_lost_outcome_is_counted_not_dropped_on_the_floor() {
        let (tx, mut rx) = mpsc::channel(4);
        let counters = std::sync::Arc::new(doorbell_rt::wake_brain::WakeBrainCounters::default());
        let fw = doorbell_rt::wake_brain::WatchForwarder::new(
            tx.clone(),
            counters.clone(),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
        );
        let retry = std::sync::Mutex::new(doorbell_rt::WatchRetry::default());
        for _ in 0..2 {
            wake_outcome_to_brain(
                WakeOutcome::EventsLost {
                    from: "seq:1".into(),
                    through: "seq:9".into(),
                },
                &fw,
                &tx,
                "ws",
                &retry,
                &std::sync::atomic::AtomicBool::new(false),
                &doorbell_rt::DiedBell::new(),
            );
        }
        let s = counters.snapshot();
        assert_eq!(s.events_lost, 2, "exact");
        assert_eq!(
            (s.dropped, s.forwarded),
            (0, 0),
            "not a drop, not a delivery"
        );
        assert!(
            rx.try_recv().is_err(),
            "no death, nothing forwarded to the brain"
        );
    }
}

#[cfg(test)]
mod inbox_flag_tests {
    /// bd-inbox-only-dead-path-0dt46: the inbox-only path is gone. Serve
    /// never hosts, and an inbox host beside a no-text daemon cannot ring
    /// NIP-17 (head=0) — running one anyway hosts nothing, silently. The
    /// no-text case refuses LOUD; a stale no-text lease folds into the full
    /// loop (acquire succeeds), which owns the timer sweep.
    #[test]
    fn no_text_holder_refuses_loud_instead_of_hosting_nothing() {
        let src = include_str!("doorbell_host.rs");
        let needle = ["fn run", "_inbox_only("].concat();
        assert!(
            !src.contains(&needle),
            "the unrunnable inbox-only path must not exist"
        );
        let a = src.find("pub fn run_embedded(").unwrap();
        let head = &src[a..a + 2500];
        assert!(
            head.contains("is_ringer(&h)"),
            "the no-text case is still distinguished from a live ringer"
        );
        assert!(
            head.contains("serve never hosts") && head.contains("leftover"),
            "the no-text refuse must name the cause and the remedy"
        );
        assert!(
            head.contains("embedded refuse double-run"),
            "other live leases still refuse"
        );
        let b = src.find("fn run_embedded_async(").unwrap();
        let tail = &src[b..(b + 15000).min(src.len())];
        assert!(
            tail.contains("spawn_sweep"),
            "the timer sweep stays with the full loop"
        );
    }

    /// The embedded host must TYPE (wave17-b): a no-text sink silently shed
    /// every wake (`injects=0 sheds=N`) while the pane was woken by nothing.
    /// The registered sink resolves pin -> pane and prompts the one plane.
    #[test]
    fn the_embedded_host_registers_the_plane_typer_and_starts_the_inbox_host() {
        let src = include_str!("doorbell_host.rs");
        let a = src.find("fn run_embedded_async(").unwrap();
        let tail = &src[a..a + 2500];
        assert!(
            tail.contains("PlanePromptSink::new("),
            "the embedded host must register the plane typer, not a no-text sink"
        );
        assert!(tail.contains("InboxHost::spawn("));
    }
}

#[cfg(test)]
mod off_runtime_tests {
    use super::*;
    use doorbell_rt::Host;

    /// Every mail-touching sync host method, called the way the brain calls
    /// them: from inside a running runtime, with IN-PROCESS mail. Inline,
    /// each of these panics "Cannot start a runtime from within a runtime".
    #[test]
    fn host_mail_methods_do_not_nest_block_on_inside_a_runtime() {
        let _home = crate::config::test_state_home();
        let _mail = crate::mail::Mail::from_config(None)
            .expect("in-process mail")
            .expect("some");
        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), "offrt-methods", "offrt", tx).expect("host");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let _ = host.sweep_maintenance();
            let _ = host.scan_ack_sla();
            let _ = host.fetch_inbox_process("SomePin", "last");
            let _ = host.tick(&BrainMsg::Watch(String::new()));
        });
    }

    /// Remote-daemon mail: the index build asks `whois` per registered pin
    /// through `backend::run_mail` (a `block_on`). Inline inside a runtime
    /// that panics, so it must go off the runtime. (The daemon is
    /// unreachable here; the failure is swallowed, only the nesting matters.)
    #[test]
    fn build_index_with_remote_mail_does_not_nest_block_on_inside_a_runtime() {
        let _home = crate::config::test_state_home();
        let proj_dir = std::env::temp_dir().join(format!("offrt-idx-{}", std::process::id()));
        std::fs::create_dir_all(&proj_dir).unwrap();
        let project = proj_dir.to_string_lossy().into_owned();
        let key = crate::state::spawn_registry_key(&project);
        let agents = config::state_dir().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join(format!("{key}.json")),
            serde_json::json!([{"pane_id": "p1", "name": "h1", "mail_identity": "PinA"}])
                .to_string(),
        )
        .unwrap();
        let cfg = Config {
            mail: Some(crate::config::MailSections {
                toron: Some(crate::config::MailServerConfig {
                    url: Some("http://127.0.0.1:9/mcp".into()),
                    token: None,
                }),
            }),
            ..Default::default()
        };
        let _mail = crate::mail::Mail::from_config(Some(&cfg.toron_mail_config()));
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let _ = build_index(&cfg, &project);
        });
    }

    /// The whole embedded loop starts with in-process mail and stays up:
    /// host construction (index), bus watch arming and the brain run all
    /// happen inside its runtime. A crash at start ends the thread.
    #[test]
    fn the_embedded_doorbell_starts_and_stays_up_with_in_process_mail() {
        let _home = crate::config::test_state_home();
        let _mail = crate::mail::Mail::from_config(None)
            .expect("in-process mail")
            .expect("some");
        let ws = "offrt-embedded";
        let cfg = Config::default();
        let h = std::thread::spawn(move || run_embedded(&cfg, "offrt-embedded", ws));
        std::thread::sleep(Duration::from_secs(4));
        let died_early = h.is_finished();
        let _ = std::fs::remove_file(doorbell::pid_file(&config::state_dir(), ws));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !h.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            !died_early,
            "the embedded doorbell exited within 4s of starting: {:?}",
            h.join().map(|r| r.map_err(|e| e.to_string()))
        );
    }
}

use toron_core::kernel::{
    BuiltServices, Effect as FxEffect, Exit, Failure, Interpret, Layer, ManagedRuntime, Program,
    ServiceTag, use_service,
};

/// The Audit arm's edge: what actually records the row. The fold never
/// touches the world; this is the only place the arm does.
pub trait AuditEdge: Send + Sync {
    fn append(&self, kind: &str, pin: &str, detail: &str) -> Result<(), String>;
}

pub struct AuditEdgeTag;

impl ServiceTag for AuditEdgeTag {
    type Service = Arc<dyn AuditEdge>;
    const ID: &'static str = "flywheel-durable-audit-edge";
}

/// Production edge: the identical stderr line handle_workspace_cmds writes
/// today (twin: src/main.rs Cmd::Audit arm). Best-effort like the original.
pub struct StderrAuditEdge;

impl AuditEdge for StderrAuditEdge {
    fn append(&self, kind: &str, pin: &str, detail: &str) -> Result<(), String> {
        eprintln!("flywheel: [Audit {kind}] {pin}: {detail}");
        Ok(())
    }
}

/// bd-bm6v: Prompt is Brain-gated (budgets, inject_allowed, sinks, digest).
/// Interpret never budgets/sinks/digests; the edge is a handle into the
/// Brain's OWN gated queue. Refusal is data (typed fail), never a drop.
pub trait PromptEdge: Send + Sync {
    fn enqueue(&self, pin: &str, line: &str) -> Result<(), String>;
}

pub struct PromptEdgeTag;

impl ServiceTag for PromptEdgeTag {
    type Service = Arc<dyn PromptEdge>;
    const ID: &'static str = "flywheel-durable-prompt-edge";
}

/// Shadow-first production: record beside the live Effect::Prompt path.
/// Cut over only when shadow enqueue set == live deliver set over the replay
/// corpus. Never duplicates delivery: the live path still owns the Brain.
/// The embedded host's terminal writer: types through the ONE selected plane
/// (rex today), resolving the pin to its pane through the spawn registry.
/// A pin with no pane is `UndeliveredPin` (loud shed), never a silent drop.
pub struct PlanePromptSink {
    cfg: Config,
    project: String,
}

impl PlanePromptSink {
    pub fn new(cfg: Config, project: &str) -> Self {
        Self {
            cfg,
            project: project.to_string(),
        }
    }
}

impl doorbell_rt::PromptSink for PlanePromptSink {
    fn send(&mut self, pin: &str, line: &str) -> Result<(), doorbell_rt::RtError> {
        let registry = state::load_spawn_registry(&self.project);
        let Some(pane) = registry
            .iter()
            .find(|e| e.mail_identity.as_deref() == Some(pin))
            .map(|e| e.pane_id.clone())
            .filter(|p| !p.is_empty())
        else {
            return Err(doorbell_rt::RtError::UndeliveredPin(pin.to_string()));
        };
        let backend = crate::backend::factory::multiplexer_backend(&self.cfg)
            .map_err(|e| doorbell_rt::RtError::PromptFailed(format!("plane: {e}")))?;
        backend
            .prompt(&pane, line, None)
            .map(|_| ())
            .map_err(|e| doorbell_rt::RtError::PromptFailed(format!("{e}")))
    }

    fn name(&self) -> &str {
        "plane"
    }
}

pub struct ShadowPromptEdge;

impl PromptEdge for ShadowPromptEdge {
    fn enqueue(&self, pin: &str, line: &str) -> Result<(), String> {
        eprintln!("flywheel: [Prompt shadow] {pin}: {line}");
        Ok(())
    }
}

/// bd-bm6v: detached spawn for FetchInbox / CheckAckSla. Serial dispatch
/// must never wait on subprocess/scan; the arm claims intent then succeeds
/// immediately, and rows re-enter through the existing pipe Msgs
/// (BrainMsg::InboxFetched / SlaFired). Unknown never settles as success.
pub trait DetachedEdge: Send + Sync {
    fn spawn(&self, kind: &str) -> Result<(), String>;
}

pub struct DetachedEdgeTag;

impl ServiceTag for DetachedEdgeTag {
    type Service = Arc<dyn DetachedEdge>;
    const ID: &'static str = "flywheel-durable-detached-edge";
}

/// Shadow-first production: record the spawn beside the live detached path.
/// The live sweep still performs the fetch/scan; this only proves the claim
/// precedes the spawn and the immediate success carries no rows.
pub struct ShadowDetachedEdge;

impl DetachedEdge for ShadowDetachedEdge {
    fn spawn(&self, kind: &str) -> Result<(), String> {
        eprintln!("flywheel: [Detached shadow] spawn {kind}");
        Ok(())
    }
}

/// Services for durable interpret (T3), built once per PROCESS, never
/// dropped: the host lives inside the async Brain, and dropping a
/// ManagedRuntime there panics (its TokioExecutor owns a tokio Runtime).
/// A process-wide static is never dropped, so the async-owned host can
/// safely borrow it. Audit + Prompt (Brain-gated) + Detached (fetch/scan).
fn durable_services() -> &'static ManagedRuntime {
    static SERVICES: std::sync::OnceLock<ManagedRuntime> = std::sync::OnceLock::new();
    SERVICES.get_or_init(|| {
        let audit = Layer::<AuditEdgeTag, std::convert::Infallible, ()>::from_value(Arc::new(
            StderrAuditEdge,
        )
            as Arc<dyn AuditEdge>);
        let prompt = Layer::<PromptEdgeTag, std::convert::Infallible, ()>::from_value(Arc::new(
            ShadowPromptEdge,
        )
            as Arc<dyn PromptEdge>);
        let detached = Layer::<DetachedEdgeTag, std::convert::Infallible, ()>::from_value(
            Arc::new(ShadowDetachedEdge) as Arc<dyn DetachedEdge>,
        );
        let layer = audit
            .merge(prompt)
            .expect("audit+prompt layers merge")
            .merge(detached)
            .expect("audit+prompt+detached layers merge");
        ManagedRuntime::make(&layer).expect("durable services build")
    })
}

pub struct DurableInterpret;

impl Interpret<crate::workspace::WorkspaceProgram> for DurableInterpret {
    fn interpret(cmd: &crate::workspace::Cmd) -> FxEffect<crate::workspace::Msg, String> {
        FxEffect::fail(format!(
            "no edge in hand for {cmd:?}: boot the host with durable services"
        ))
    }

    fn interpret_in_env(
        cmd: &crate::workspace::Cmd,
        services: Option<&BuiltServices>,
    ) -> FxEffect<crate::workspace::Msg, String> {
        let quiet = || crate::workspace::Msg::DiagnoseFailed {
            name: crate::workspace::SHADOW_FAILURE_NAME.into(),
        };
        match cmd {
            crate::workspace::Cmd::Audit { kind, pin, detail } => {
                let Some(services) = services else {
                    return Self::interpret(cmd);
                };
                let (kind, pin, detail) = (kind.clone(), pin.clone(), detail.clone());
                let q = quiet();
                use_service::<AuditEdgeTag>()
                    .map_err(|_: std::convert::Infallible| String::from("unreachable"))
                    .flat_map(move |edge| match edge.append(&kind, &pin, &detail) {
                        Ok(()) => FxEffect::succeed(q.clone()),
                        Err(detail) => FxEffect::fail(detail),
                    })
                    .provide_from(services)
            }
            crate::workspace::Cmd::Prompt { pin, line } => {
                let Some(services) = services else {
                    return Self::interpret(cmd);
                };
                let (pin, line) = (pin.clone(), line.clone());
                let q = quiet();
                use_service::<PromptEdgeTag>()
                    .map_err(|_: std::convert::Infallible| String::from("unreachable"))
                    .flat_map(move |edge| match edge.enqueue(&pin, &line) {
                        Ok(()) => FxEffect::succeed(q.clone()),
                        Err(refused) => FxEffect::fail(refused),
                    })
                    .provide_from(services)
            }
            crate::workspace::Cmd::FetchInbox | crate::workspace::Cmd::CheckAckSla => {
                let Some(services) = services else {
                    return Self::interpret(cmd);
                };
                let kind: &'static str = match cmd {
                    crate::workspace::Cmd::FetchInbox => "fetch_inbox",
                    _ => "check_ack_sla",
                };
                let q = quiet();
                use_service::<DetachedEdgeTag>()
                    .map_err(|_: std::convert::Infallible| String::from("unreachable"))
                    .flat_map(move |edge| match edge.spawn(kind) {
                        Ok(()) => FxEffect::succeed(q.clone()),
                        Err(refused) => FxEffect::fail(refused),
                    })
                    .provide_from(services)
            }
            _ => Self::interpret(cmd),
        }
    }
}

#[cfg(test)]
mod durable_tests {
    use super::*;
    use toron_core::kernel::{Exit, Runtime};
    use workspace::{Msg, SHADOW_FAILURE_NAME};

    struct Recorder {
        ran: std::sync::Mutex<Vec<(String, String, String)>>,
        refuse: bool,
    }

    impl AuditEdge for Recorder {
        fn append(&self, kind: &str, pin: &str, detail: &str) -> Result<(), String> {
            self.ran
                .lock()
                .unwrap()
                .push((kind.into(), pin.into(), detail.into()));
            if self.refuse {
                Err("the edge refused".into())
            } else {
                Ok(())
            }
        }
    }

    struct PromptRecorder {
        ran: std::sync::Mutex<Vec<(String, String)>>,
        refuse: bool,
    }

    impl PromptEdge for PromptRecorder {
        fn enqueue(&self, pin: &str, line: &str) -> Result<(), String> {
            self.ran.lock().unwrap().push((pin.into(), line.into()));
            if self.refuse {
                Err("prompt gate refused".into())
            } else {
                Ok(())
            }
        }
    }

    struct DetachedRecorder {
        ran: std::sync::Mutex<Vec<String>>,
        refuse: bool,
    }

    impl DetachedEdge for DetachedRecorder {
        fn spawn(&self, kind: &str) -> Result<(), String> {
            self.ran.lock().unwrap().push(kind.into());
            if self.refuse {
                Err("spawn refused".into())
            } else {
                Ok(())
            }
        }
    }

    fn services_with(
        refuse: bool,
    ) -> (
        ManagedRuntime,
        Arc<Recorder>,
        Arc<PromptRecorder>,
        Arc<DetachedRecorder>,
    ) {
        let rec = Arc::new(Recorder {
            ran: std::sync::Mutex::new(vec![]),
            refuse,
        });
        let prompt = Arc::new(PromptRecorder {
            ran: std::sync::Mutex::new(vec![]),
            refuse,
        });
        let detached = Arc::new(DetachedRecorder {
            ran: std::sync::Mutex::new(vec![]),
            refuse,
        });
        let audit_layer = Layer::<AuditEdgeTag, std::convert::Infallible, ()>::from_value(
            rec.clone() as Arc<dyn AuditEdge>,
        );
        let prompt_layer = Layer::<PromptEdgeTag, std::convert::Infallible, ()>::from_value(
            prompt.clone() as Arc<dyn PromptEdge>,
        );
        let detached_layer = Layer::<DetachedEdgeTag, std::convert::Infallible, ()>::from_value(
            detached.clone() as Arc<dyn DetachedEdge>,
        );
        let layer = audit_layer
            .merge(prompt_layer)
            .expect("test audit+prompt merge")
            .merge(detached_layer)
            .expect("test audit+prompt+detached merge");
        let rt = ManagedRuntime::make(&layer).expect("value layer builds");
        assert!(rt.context().is_some());
        (rt, rec, prompt, detached)
    }

    fn audit_cmd() -> workspace::Cmd {
        workspace::Cmd::Audit {
            kind: "k".into(),
            pin: "p".into(),
            detail: "d".into(),
        }
    }

    #[test]
    fn audit_arm_runs_through_the_edge() {
        let (rt, rec, _, _) = services_with(false);
        let exit = Runtime::run(DurableInterpret::interpret_in_env(
            &audit_cmd(),
            rt.context(),
        ));
        let Exit::Success(msg) = exit else {
            panic!("edge success must complete quiet");
        };
        assert_eq!(
            rec.ran.lock().unwrap().as_slice(),
            &[("k".to_string(), "p".to_string(), "d".to_string())]
        );
        assert!(matches!(msg, Msg::DiagnoseFailed { .. }));
        let mut model = workspace::WorkspaceModel::default();
        model.swarm.heal_states.insert(
            "probe".into(),
            crate::durable::HealState {
                awaiting_clear: true,
                ..Default::default()
            },
        );
        let (next, cmds) = workspace::tick(&model, &msg);
        assert!(cmds.is_empty());
        assert!(
            next.swarm
                .heal_states
                .get("probe")
                .expect("kept")
                .awaiting_clear,
            "a quiet fold preserves the model"
        );
    }

    #[test]
    fn edge_refusal_surfaces_typed_failure_named_by_the_fold() {
        let (rt, rec, _, _) = services_with(true);
        let exit = Runtime::run(DurableInterpret::interpret_in_env(
            &audit_cmd(),
            rt.context(),
        ));
        let Exit::Failure(cause) = exit else {
            panic!("edge refusal must fail typed, never succeed");
        };
        assert_eq!(rec.ran.lock().unwrap().len(), 1);
        let msg = workspace::WorkspaceProgram::failure(&Failure::of_cause(cause));
        assert!(matches!(msg, Msg::DiagnoseFailed { ref name } if name == SHADOW_FAILURE_NAME));
        let mut model = workspace::WorkspaceModel::default();
        model.swarm.heal_states.insert(
            "probe".into(),
            crate::durable::HealState {
                awaiting_clear: true,
                ..Default::default()
            },
        );
        let (next, cmds) = workspace::tick(&model, &msg);
        assert!(cmds.is_empty());
        assert!(
            next.swarm
                .heal_states
                .get("probe")
                .expect("kept")
                .awaiting_clear,
            "a quiet fold preserves the model"
        );
    }

    #[test]
    fn no_edge_in_hand_fails_typed_never_drops() {
        let exit = Runtime::run(DurableInterpret::interpret(&audit_cmd()));
        assert!(
            !exit.is_success(),
            "no edge in hand must fail typed, never drop"
        );
        let exit = Runtime::run(DurableInterpret::interpret_in_env(&audit_cmd(), None));
        assert!(!exit.is_success());
    }

    #[test]
    fn non_audit_commands_are_not_owned_by_this_arm() {
        let (rt, rec, prompt_rec, detached_rec) = services_with(false);
        let other = workspace::Cmd::Send {
            name: "n".into(),
            text: "t".into(),
        };
        let exit = Runtime::run(DurableInterpret::interpret_in_env(&other, rt.context()));
        assert!(!exit.is_success(), "unowned arms fail typed, never run");
        assert!(rec.ran.lock().unwrap().is_empty());
        assert!(prompt_rec.ran.lock().unwrap().is_empty());
        assert!(detached_rec.ran.lock().unwrap().is_empty());
    }

    #[test]
    fn services_builder_builds_once_and_carries_the_edge() {
        let a = durable_services();
        let b = durable_services();
        assert!(std::ptr::eq(a, b), "one runtime per process, never rebuilt");
        assert!(a.context().is_some());
    }

    fn quiet_preserves_model(msg: &Msg) {
        let mut model = workspace::WorkspaceModel::default();
        model.swarm.heal_states.insert(
            "probe".into(),
            crate::durable::HealState {
                awaiting_clear: true,
                ..Default::default()
            },
        );
        let (next, cmds) = workspace::tick(&model, msg);
        assert!(cmds.is_empty());
        assert!(
            next.swarm
                .heal_states
                .get("probe")
                .expect("kept")
                .awaiting_clear,
            "a quiet fold preserves the model"
        );
    }

    #[test]
    fn prompt_arm_enqueue_ok_succeeds_quiet() {
        let (rt, _, prompt_rec, detached_rec) = services_with(false);
        let cmd = workspace::Cmd::Prompt {
            pin: "p".into(),
            line: "[doorbell] hello".into(),
        };
        let exit = Runtime::run(DurableInterpret::interpret_in_env(&cmd, rt.context()));
        let Exit::Success(msg) = exit else {
            panic!("bd-bm6v: Prompt with services must succeed quiet via PromptEdge");
        };
        assert!(matches!(msg, Msg::DiagnoseFailed { .. }));
        assert_eq!(
            prompt_rec.ran.lock().unwrap().as_slice(),
            &[("p".to_string(), "[doorbell] hello".to_string())]
        );
        assert!(detached_rec.ran.lock().unwrap().is_empty());
        quiet_preserves_model(&msg);
    }

    #[test]
    fn prompt_arm_gate_refused_fails_typed() {
        let (rt, _, prompt_rec, _) = services_with(true);
        let cmd = workspace::Cmd::Prompt {
            pin: "p".into(),
            line: "[doorbell] hello".into(),
        };
        let exit = Runtime::run(DurableInterpret::interpret_in_env(&cmd, rt.context()));
        let Exit::Failure(cause) = exit else {
            panic!("prompt gate refusal must fail typed, never succeed");
        };
        assert_eq!(prompt_rec.ran.lock().unwrap().len(), 1);
        let msg = workspace::WorkspaceProgram::failure(&Failure::of_cause(cause));
        assert!(matches!(msg, Msg::DiagnoseFailed { ref name } if name == SHADOW_FAILURE_NAME));
    }

    #[test]
    fn prompt_arm_no_services_fails_typed() {
        let cmd = workspace::Cmd::Prompt {
            pin: "p".into(),
            line: "[doorbell] hello".into(),
        };
        assert!(!Runtime::run(DurableInterpret::interpret(&cmd)).is_success());
        assert!(!Runtime::run(DurableInterpret::interpret_in_env(&cmd, None)).is_success());
    }

    #[test]
    fn detached_fetch_spawns_and_succeeds_immediately() {
        let (rt, audit_rec, _, detached_rec) = services_with(false);
        let exit = Runtime::run(DurableInterpret::interpret_in_env(
            &workspace::Cmd::FetchInbox,
            rt.context(),
        ));
        let Exit::Success(msg) = exit else {
            panic!("bd-bm6v: FetchInbox with services must succeed immediately via DetachedEdge");
        };
        assert!(matches!(msg, Msg::DiagnoseFailed { .. }));
        assert_eq!(
            detached_rec.ran.lock().unwrap().as_slice(),
            &["fetch_inbox".to_string()]
        );
        assert!(audit_rec.ran.lock().unwrap().is_empty());
        quiet_preserves_model(&msg);
    }

    #[test]
    fn detached_ack_sla_spawns_and_succeeds_immediately() {
        let (rt, _, _, detached_rec) = services_with(false);
        let exit = Runtime::run(DurableInterpret::interpret_in_env(
            &workspace::Cmd::CheckAckSla,
            rt.context(),
        ));
        let Exit::Success(msg) = exit else {
            panic!("bd-bm6v: CheckAckSla with services must succeed immediately via DetachedEdge");
        };
        assert!(matches!(msg, Msg::DiagnoseFailed { .. }));
        assert_eq!(
            detached_rec.ran.lock().unwrap().as_slice(),
            &["check_ack_sla".to_string()]
        );
        quiet_preserves_model(&msg);
    }

    #[test]
    fn detached_spawn_refused_fails_typed() {
        let (rt, _, _, detached_rec) = services_with(true);
        let exit = Runtime::run(DurableInterpret::interpret_in_env(
            &workspace::Cmd::FetchInbox,
            rt.context(),
        ));
        let Exit::Failure(cause) = exit else {
            panic!("spawn refusal must fail typed, never succeed");
        };
        assert_eq!(detached_rec.ran.lock().unwrap().len(), 1);
        let msg = workspace::WorkspaceProgram::failure(&Failure::of_cause(cause));
        assert!(matches!(msg, Msg::DiagnoseFailed { ref name } if name == SHADOW_FAILURE_NAME));
    }

    #[test]
    fn detached_no_services_fails_typed() {
        assert!(
            !Runtime::run(DurableInterpret::interpret(&workspace::Cmd::FetchInbox)).is_success()
        );
        assert!(
            !Runtime::run(DurableInterpret::interpret_in_env(
                &workspace::Cmd::FetchInbox,
                None
            ))
            .is_success()
        );
        assert!(
            !Runtime::run(DurableInterpret::interpret(&workspace::Cmd::CheckAckSla)).is_success()
        );
        assert!(
            !Runtime::run(DurableInterpret::interpret_in_env(
                &workspace::Cmd::CheckAckSla,
                None
            ))
            .is_success()
        );
    }

    #[test]
    fn restart_and_factmiss_stay_on_the_live_path() {
        let (rt, _, prompt_rec, detached_rec) = services_with(false);
        for cmd in [
            workspace::Cmd::RestartWatch,
            workspace::Cmd::FactMiss { why: "w".into() },
        ] {
            let exit = Runtime::run(DurableInterpret::interpret_in_env(&cmd, rt.context()));
            assert!(
                !exit.is_success(),
                "live-path arms stay unowned by interpret"
            );
        }
        assert!(prompt_rec.ran.lock().unwrap().is_empty());
        assert!(detached_rec.ran.lock().unwrap().is_empty());
    }
}

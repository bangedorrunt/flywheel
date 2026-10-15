// governed-by: ADR-0012
//! Embedded host binding (bd-asr2.5): [`RtHost`] implements
//! `doorbell_rt::Host` over the live flywheel machinery — `workspace::tick`
//! as the decider, `handle_workspace_cmds` as the effect layer, the spawn
//! registry / mailbox / herdr shims as producers. [`run_embedded`] wires
//! sensors around a `doorbell_rt::Brain` and runs it to shutdown.
//!
//! One implementation: every arm below mirrors `cmd_internal_doorbell`
//! (removed); behavioral deltas are marked DELTA with their reason.

use crate::config::Config;
use crate::herdr_shim::{Client, StreamOutcome, Subscription};
use crate::{config, doorbell, state, workspace};
use doorbell_rt::{BrainMsg, Effect, FileStore, Host};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// herdr-prompt sink adapter: the live `doorbell_prompt` delivery path
/// (gen stamp, pa/jcode fallback, unknown-pin loud miss) behind the
/// `PromptSink` trait. Empty delivery (unknown pin) is an Err so the brain
/// sheds it to digest loudly instead of stamping hear on air.
pub struct HerdrPromptSink {
    project: String,
    workspace: String,
    generation: Option<doorbell::WatcherGen>,
    client: Option<Client>,
}

impl HerdrPromptSink {
    pub fn new(
        cfg: &Config,
        project: &str,
        workspace: &str,
        generation: Option<doorbell::WatcherGen>,
    ) -> Self {
        Self {
            project: project.to_string(),
            workspace: workspace.to_string(),
            generation,
            client: Client::connect(Some(&cfg.socket_path())).ok(),
        }
    }
}

impl doorbell_rt::PromptSink for HerdrPromptSink {
    fn send(&mut self, pin: &str, line: &str) -> Result<(), doorbell_rt::RtError> {
        let registry = state::load_spawn_registry(&self.project);
        let delivered = crate::doorbell_prompt(
            self.client.as_mut(),
            &registry,
            &[(pin.to_string(), line.to_string())],
            self.generation.as_ref(),
            &self.project,
            &self.workspace,
        );
        if delivered.is_empty() {
            return Err(doorbell_rt::RtError::UndeliveredPin(pin.to_string()));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "herdr"
    }
}

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
    /// the next sweep re-arms (herdr/tuios: stream death must not leave a
    /// pane stuck "covered" forever).
    covered_panes: Arc<Mutex<HashSet<String>>>,
    /// Shared with the delivery path. bd-kz54 follow-up: `WatchRetry::reset()`
    /// had exactly one caller in the whole tree — its own unit test — so four
    /// failures anywhere in a host's life latched the 16s gate permanently and
    /// the transport never regained a fast first retry.
    retry: std::sync::Arc<std::sync::Mutex<doorbell_rt::WatchRetry>>,
    authority: crate::authority::RuntimeAuthority,
    // F7: cache the whois-backed index — rebuild at most every 60s or when
    // the registry pin set changes (was every 1s sweep: N whois RPCs/sec).
    index_refreshed_at: u64,
    index_pins: Vec<String>,
    /// The one live wake-socket consumer. Shared with the restart task so a
    /// replacement aborts its predecessor instead of littering watchers.
    wake_task: std::sync::Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>,
    /// mcc9: exact wake → brain counters for this workspace, shared by every
    /// watch source this host arms (bus watch and each wake-sock consumer).
    wake_counters: Arc<doorbell_rt::wake_brain::WakeBrainCounters>,
    /// mcc9: the last line the brain accepted; a replacement consumer resumes
    /// there instead of replaying from origin.
    wake_resume: Arc<Mutex<Option<String>>>,
    /// bd-b2b3: the run-state recorder, `None` unless explicitly attached.
    /// The recorder is an EFFECT-layer component (it owns a store handle and a
    /// clock) and is therefore held HERE, beside the model, and never touched
    /// from a reducer. `None` means "no recorder", which is visible rather than
    /// mistaken for "no boundaries recorded yet".
    state_log: Option<crate::run_state_recorder::RunStateRecorder>,
    /// The writer thread draining `state_log`. Held so the handle is not
    /// dropped; the thread runs for the process lifetime.
    // The field is never READ, and that is the point: holding the JoinHandle is
    // what keeps the writer's result observable and prevents an accidental
    // detach. The thread is intentionally not shut down per-host.
    #[allow(dead_code)]
    state_log_writer: Option<std::thread::JoinHandle<()>>,
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
) {
    use doorbell_rt::wake_brain::Forwarded;
    use doorbell_rt::wake_sock::WakeOutcome;
    // mcc9: Watch goes through the forwarder (backpressure on a full channel,
    // then an exact-count drop + ONE death + cursor resume — never one death
    // per line, never a silent shed). Stamp .event only once the brain has the
    // line; Died via blocking_send so Full cannot drop restart_wake.
    match out {
        WakeOutcome::Watch(l) => match fw.forward(l) {
            f if f.reached_brain() => {
                doorbell_rt::watchbeat::touch_event(&config::state_dir(), workspace);
                // A delivered event is the only trustworthy evidence the socket
                // is live again, so it is the only thing that may reopen the
                // backoff gate. A heartbeat proves the ticker ran, not that the
                // transport delivers — bd-kz54's whole lesson.
                if let Ok(mut r) = retry.lock() {
                    r.reset();
                }
            }
            Forwarded::Dropped {
                report_death: true, ..
            } => doorbell_rt::msg::deliver_died_blocking(tx),
            // Counted by the forwarder; the consumer is already condemned.
            _ => {}
        },
        WakeOutcome::Heartbeat => {
            doorbell_rt::watchbeat::touch_heartbeat(&config::state_dir(), workspace);
        }
        // toron#17: a counted, audited gap (the consumer wrote the row); the
        // consumer is not dead and nothing reconnects.
        WakeOutcome::EventsLost { .. } => fw.note_events_lost(),
        WakeOutcome::Died => {
            doorbell_rt::msg::deliver_died_blocking(tx);
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
        // GS-048: restore sticky-open topology across host restarts.
        if let Some(topo) = state::load_session_topology(project) {
            model.session_topology = Some(topo);
        }
        // Same seeds as the old loop head (fail-loud on I/O, never skip).
        // Host construction runs inside the embedded runtime; the receipt
        // store lists via `block_on` on the shared mail runtime, so seed off it.
        crate::backend::run_mail_off_runtime_sync(|| -> anyhow::Result<()> {
            crate::durable::seed_workspace_from_active_loop(&mut model, project)?;
            crate::seed_roles_from_binds(&mut model, project)?;
            crate::rebuild_assignment_from_receipts(&mut model);
            Ok(())
        })?;
        model.last_now = unix_now();
        // bd-b2b3: opt-in. Recording a boundary per tick changes the
        // dispatcher's storage and latency profile, so it is an explicit
        // operator choice (FLYWHEEL_RUN_STATE_LOG=1) rather than a silent new
        // default. `attach` returns None when no toron index is open, which is
        // also visible rather than a silent no-op.
        let state_log = match std::env::var("FLYWHEEL_RUN_STATE_LOG") {
            Ok(v) if v.trim() == "1" || v.trim().eq_ignore_ascii_case("true") => {
                crate::run_state_recorder::RunStateRecorder::attach(&format!("flywheel/{project}"))
            }
            _ => None,
        };
        // bd-b2b3: the writer task is what makes an enqueued boundary durable.
        // Without it the queue fills to PENDING_CAP and then refuses loudly
        // forever, so the recorder is only ever constructed together with its
        // writer. Computed BEFORE the struct literal because the literal moves
        // `state_log`.
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
            retry: std::sync::Arc::new(std::sync::Mutex::new(doorbell_rt::WatchRetry::default())),
            authority: crate::authority::RuntimeAuthority::resolve(),
            index_refreshed_at: now,
            index_pins,
            wake_task: std::sync::Arc::new(std::sync::Mutex::new(None)),
            wake_counters: doorbell_rt::wake_brain::counters_for(workspace),
            wake_resume: Arc::new(Mutex::new(None)),
            // bd-b2b3: opt-in. Recording a boundary per tick changes the
            // dispatcher's storage and latency profile, so it is an explicit
            // operator choice (`FLYWHEEL_RUN_STATE_LOG=1`) rather than a silent
            // new default. Returns None when no toron index is open, which is
            // also visible rather than a silent no-op.
            state_log,
            state_log_writer,
        })
    }

    pub fn set_generation(&mut self, generation: Option<doorbell::WatcherGen>) {
        self.generation = generation;
    }

    fn registry_pins(&self) -> Vec<String> {
        state::load_spawn_registry(&self.project)
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect()
    }

    /// Spawn push forwarders for registry panes without coverage (bd-8hv0):
    /// late crew panes get push coverage instead of the polling fallback.
    ///
    /// Multiplex: one `events.subscribe` covering all uncovered crew panes
    /// (herdr/tuios per-caller stream cap). On stream death, clear covered
    /// so the next sweep re-arms. If subscribe rejects because a listed pane
    /// died, refresh the pane list and retry. `events_lost` stays TODO until
    /// toron-core surfaces it as a typed signal (see hook below).
    fn cover_new_panes(&mut self) {
        let registry = state::load_spawn_registry(&self.project);
        let covered = self
            .covered_panes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
            let Ok(mut sub_client) = Client::connect(Some(&socket)) else {
                return;
            };
            let subs = Subscription::agent_status_changed_many(attempt_ids.iter().cloned());
            match crate::herdr_shim::subscribe(&mut sub_client, &subs) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => {
                    // herdr rejects the whole request if any listed pane is gone.
                    eprintln!(
                        "flywheel doorbell: multiplex subscribe rejected ({e:#}) — refreshing pane list"
                    );
                    let live: HashSet<String> = Client::connect(Some(&socket))
                        .ok()
                        .and_then(|c| c.pane_list(None).ok())
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
            let mut covered = self
                .covered_panes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            covered.extend(attempt_ids.iter().cloned());
        }

        let covered_shared = Arc::clone(&self.covered_panes);
        let clear_ids = attempt_ids.clone();
        let socket_for_reconcile = socket.clone();
        std::thread::spawn(move || {
            loop {
                match stream.read_outcome() {
                    Ok(StreamOutcome::Event(ev)) => {
                        if let crate::herdr_shim::HerdrEventData::PaneAgentStatusChanged(change)
                        | crate::herdr_shim::HerdrEventData::PaneAgentStatusChangedSub(change) =
                            ev.data
                        {
                            let pin = pin_by_pane
                                .get(&change.pane_id)
                                .cloned()
                                .unwrap_or_else(|| change.pane_id.clone());
                            let msg = BrainMsg::HerdrStatus {
                                pin,
                                status: change.agent_status.as_str().to_string(),
                            };
                            if tx.blocking_send(msg).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(StreamOutcome::EventsLost) => {
                        // Typed events_lost: clear covered, snapshot-reconcile,
                        // then exit so the next sweep re-subscribes.
                        eprintln!(
                            "flywheel doorbell: events_lost — clearing coverage and reconciling via session.snapshot"
                        );
                        clear_covered(&covered_shared, &clear_ids);
                        reconcile_status_after_events_lost(
                            &socket_for_reconcile,
                            &pin_by_pane,
                            &tx,
                        );
                        break;
                    }
                    Ok(StreamOutcome::Closed) | Err(_) => {
                        // Stream death: clear coverage so the next sweep re-arms.
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
/// (fall back to `agent.list`) and re-enter as HerdrStatus Msgs. Does not
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
        Ok(snap) => agents_from_session_snapshot(&snap).unwrap_or_else(|| {
            c.agent_list().unwrap_or_default()
        }),
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
        let msg = BrainMsg::HerdrStatus {
            pin: pin.clone(),
            status: ag.agent_status.as_str().to_string(),
        };
        if tx.blocking_send(msg).is_err() {
            break;
        }
    }
}

/// Pull AgentInfo rows out of a session.snapshot payload when present.
fn agents_from_session_snapshot(
    snap: &serde_json::Value,
) -> Option<Vec<crate::herdr_shim::AgentInfo>> {
    // Common shapes: { agents: [...] } or { panes: [...] } with agent fields,
    // or nested under tabs. Prefer an agents array; else try panes.
    if let Some(arr) = snap.get("agents").and_then(|v| v.as_array()) {
        let parsed: Result<Vec<_>, _> = arr
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect();
        return parsed.ok();
    }
    if let Some(arr) = snap.get("panes").and_then(|v| v.as_array()) {
        let parsed: Result<Vec<_>, _> = arr
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect();
        return parsed.ok();
    }
    None
}

fn build_index(cfg: &Config, project: &str) -> doorbell::SelfIndex {
    let pins: Vec<String> = state::load_spawn_registry(project)
        .iter()
        .filter_map(|e| e.mail_identity.clone())
        .collect();
    // Mail whois runs `block_on` on the shared mail runtime; this is reached
    // from the brain's runtime thread (host construction, maintenance sweep).
    crate::backend::run_mail_off_runtime_sync(|| crate::doorbell_self_index(cfg, project, &pins))
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
            BrainMsg::HerdrStatus { pin, status } => (
                workspace::Msg::HerdrStatus {
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
            // GS-048: material Fact (AUTHORITATIVE / goal change / explicit
            // topology / …) enters Msg::MaterialContext so sticky-open and
            // DecisionReady ride the Elm plane. Routine facts stay Prompt-only.
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
            // bd-w98c residual: the scan's hit list IS a reducer Msg now —
            // it audits the delta and asks for the inbox whose rows register
            // the ladder. The wake itself still arrives via the watch path
            // (toron emits real channel posts, exactly like the old loop).
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
        // The tick is the decider; the effect layer runs, then intents map.
        let (next, cmds) = workspace::tick(&self.model, &wmsg);
        self.model = next;
        // GS-048: expand Reclassify → DecisionReady cmds so Prompt announces
        // reach Effect::Prompt (nested handle would drop them). Live Snapshot
        // from briefing world facts — never Snapshot::default() invent-green.
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
        crate::handle_workspace_cmds(
            &mut self.model,
            &cmds,
            &self.project,
            &self.cfg,
            drain_waits,
        );
        // bd-b2b3: record the boundary HERE, AFTER the effect layer.
        //
        // This placement is load-bearing and easy to get wrong. `tick` is pure
        // and returns at the fold boundary, but `handle_workspace_cmds` above
        // MUTATES the model — so a snapshot taken when `tick` returned would
        // record an incomplete model that cannot resume. The only correct point
        // is after the effect layer has finished, because that is the state the
        // NEXT tick will read.
        //
        // `enqueue`, not `record`: `Host::tick` is SYNCHRONOUS, so the store
        // write cannot be awaited here. The boundary is serialized on this
        // thread and handed to the bounded queue a writer task drains. A refusal
        // is LOGGED, never swallowed: a silently skipped boundary would make a
        // later replay look like it lost history for no reason.
        if let Some(recorder) = self.state_log.as_ref()
            && let Err(refusal) = recorder.enqueue(&self.model, &[])
        {
            eprintln!("flywheel: run-state boundary not recorded: {refusal}");
        }
        cmds.into_iter()
            .filter_map(|c| match c {
                workspace::Cmd::Prompt { pin, line } => Some(Effect::Prompt { pin, line }),
                // Pin is advisory: the brain fetches every known pin with
                // per-pin cursors (cursor-safe), matching the old sweep arm.
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
        let Some(mb) = crate::mail_shim::try_mailbox() else {
            return Vec::new();
        };
        match crate::backend::run_mail_off_runtime_sync(|| {
            crate::mail_shim::block_on(mb.check_ack_sla_expired(&self.project, None))
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
        // F7: cached index — registry pin-set change or 60s TTL triggers the
        // whois-backed rebuild; steady-state sweeps skip N RPCs/sec.
        let pins_now: Vec<String> = self.registry_pins();
        let now = unix_now();
        if pins_now != self.index_pins || now.saturating_sub(self.index_refreshed_at) >= 60 {
            self.index = build_index(&self.cfg, &self.project);
            self.index_pins = pins_now;
            self.index_refreshed_at = now;
        }
        self.cover_new_panes();
        // Presence hear: presence.jsonl is the real projection for ephemeral
        // 20001 (bus never stores it). Trust root stays the tick (Brain
        // ticks Presence per row; hear derives after). The path is toron's
        // canonical root-level projection (`mailbox_root` lift, bd-m5ym) —
        // the projects-level file is never written and starves the map.
        let path = crate::mail_shim::presence_path();
        let Ok(body) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let pins: Vec<String> = self.registry_pins();
        crate::timeout_presence_rows(&body, unix_now())
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
        // GS-007 never-deaf rule: burst in a task, gate across calls.
        let backoff = self.retry.lock().map(|mut r| r.record_fail()).unwrap_or(16);
        let tx = self.brain_tx.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let slot = self.wake_task.clone();
        // Hoisted out of the 'static future so no borrow of `self` escapes.
        let retry = self.retry.clone();
        let counters = self.wake_counters.clone();
        let resume = self.wake_resume.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            // Abort the predecessor first: one live consumer per host.
            if let Ok(mut g) = slot.lock()
                && let Some(h) = g.take()
            {
                h.abort();
            }
            let tx_g = tx.clone();
            let beat_workspace = workspace.clone();
            let retry = retry.clone();
            // mcc9: resume where the brain last accepted a line.
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
            let handle = crate::spawn_wake_consumer(
                &project,
                &workspace,
                &config::state_dir(),
                resume_from,
                move |out| {
                    wake_outcome_to_brain(out, &fw, &tx_g, &beat_workspace, &retry);
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
        // In-process mail `block_on`s the shared mail runtime inside
        // `inbox_filtered`; this is called from the brain's runtime thread.
        crate::backend::run_mail_off_runtime_sync(|| {
            let mail = crate::mail_shim::Mail::from_config(Some(&self.cfg.toron_mail_config()))
                .ok()
                .flatten()?;
            let filter = crate::mail_shim::MailInboxFilter {
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
            .args(crate::mail_inbox_fallback_argv(pin, &self.project, since))
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
    // F3/bd-ene2 D9 layer 1 — claim the single-writer lease for THIS
    // workspace before starting (per-ws scope: a second workspace's loop
    // is independent). Live lease held elsewhere for the same workspace
    // means another loop runs: refuse, never double-run.
    let now = doorbell_rt::lease::unix_now();
    let lease =
        doorbell_rt::lease::acquire_lease(&config::state_dir(), workspace, now).map_err(|e| {
            anyhow::anyhow!("flywheel doorbell: embedded refuse double-run (live lease): {e:?}")
        })?;
    let path = doorbell::pid_file(&config::state_dir(), workspace);
    doorbell::write_pid(&path, std::process::id())?;
    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
    let gen_path = doorbell::gen_file(&config::state_dir(), workspace);
    let generation = match doorbell::read_gen(&gen_path) {
        Some(record) => Some(record),
        None => doorbell::WatcherGen::capture(&exe_path, unix_now())
            .filter(|record| doorbell::write_gen(&gen_path, record).is_ok()),
    };
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
    // Release our claim only (never drop a stranger's lease).
    doorbell_rt::lease::release_lease(&config::state_dir(), workspace, &lease);
    res
}

async fn run_embedded_async(
    cfg: Config,
    project: &str,
    workspace: &str,
    generation: Option<doorbell::WatcherGen>,
) -> anyhow::Result<()> {
    use doorbell_rt::{Brain, BrainConfig, SinkManager};
    let (tx, rx) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
    let mut host = RtHost::new(cfg.clone(), project, workspace, tx.clone())?;
    host.set_generation(generation.clone());
    let mut sinks = SinkManager::new(tx.clone());
    // Supervised registration (audit P1): fresh adapter + reconnect on a
    // failed herdr client park (Transient policy) instead of dying until
    // process restart. The generation stamps the fresh adapter so a
    // mid-run binary upgrade still self-retires it.
    let (fp, fw, fgen) = (
        project.to_string(),
        workspace.to_string(),
        generation.clone(),
    );
    let cfg_for_factory = cfg.clone();
    sinks.add_sink_with_factory(
        "herdr",
        Box::new(HerdrPromptSink::new(
            &cfg,
            project,
            workspace,
            generation.clone(),
        )),
        move || {
            Box::new(HerdrPromptSink::new(
                &cfg_for_factory,
                &fp,
                &fw,
                fgen.clone(),
            ))
        },
    );
    let store = FileStore::new(config::state_dir());
    // Watch source: in-process bus watch first (zero fork), wake-sock fallback.
    // Both stamp the watchbeat (event/hb split, GS-062); the socket task goes
    // through the host's slot so the restart path aborts its predecessor.
    let watch_slot = host.wake_task.clone();
    let bus_fw = wake_forwarder(
        &tx,
        workspace,
        host.wake_counters.clone(),
        host.wake_resume.clone(),
    );
    // `spawn_bus_watch` drives the shared mail runtime with `block_on`; run it
    // off this runtime (backend::run_mail_off_runtime).
    let bus_armed = {
        let (cfg_b, project_b, workspace_b, tx_b) = (
            cfg.clone(),
            project.to_string(),
            workspace.to_string(),
            tx.clone(),
        );
        crate::backend::run_mail_off_runtime(move || {
            Ok(spawn_bus_watch(
                &cfg_b,
                &project_b,
                &workspace_b,
                tx_b,
                bus_fw,
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
        let handle = crate::spawn_wake_consumer(
            &project,
            workspace,
            &config::state_dir(),
            None,
            move |out| {
                wake_outcome_to_brain(out, &fw, &tx_w, &beat_workspace, &retry_w);
            },
        );
        if let Ok(mut g) = watch_slot.lock() {
            *g = Some(handle);
        }
    }
    host.cover_new_panes();
    // Beats: 1s sweep, 60s ack-SLA (same cadences as the old loop).
    let sweep_tx = tx.clone();
    let sweep_ticker =
        doorbell_rt::interval_ticker("sweep", sweep_tx, Duration::from_secs(1), || {
            BrainMsg::Sweep { now: unix_now() }
        });
    // F3/bd-ene2 D9 heartbeat — refresh our workspace lease every 30s so
    // a crash goes stale within 90s and a wedged writer cannot hold it.
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
    let sla_tx = tx.clone();
    let _sla_ticker =
        doorbell_rt::interval_ticker("ack-sla", sla_tx, Duration::from_secs(60), || {
            BrainMsg::AckSlaTick { now: unix_now() }
        });
    // Beads-ledger producer (bd-asr2.15, D7; ready-bead lane
    // bd-ux-papercuts-t2u5): read-only WAL poll emitting Fact Msgs; default
    // on, `beads_poller = false` opts out. Repo resolution is flywheel's own
    // `project_dir` (absolute key = the repo, slug = workspace_root/<slug>) —
    // never the process cwd, which says nothing about the project.
    let _beads_poller = cfg.doorbell_beads_poller().then(|| {
        let repo = crate::project_dir(&cfg, project);
        doorbell_rt::pollers::spawn_beads_poller(tx.clone(), repo)
    });
    // External re-ring driver (E9, bd-4gco, flag-gated): read-only
    // foreign-close watch emitting Fact Msgs. Same-host only; the daemon
    // host gains the same spawn when the Phase-3 Brain loop lands there.
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
    let shutdown_tx = tx.clone();
    let _liveness = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !pid_path.exists() {
                break;
            }
            if let Some(record) = doorbell::read_gen(&gen_path)
                && !doorbell::inject_allowed(&record, &exe)
            {
                eprintln!(
                    "flywheel doorbell: binary changed since watcher start; retiring without further wakes"
                );
                break;
            }
        }
        let _ = shutdown_tx.send(BrainMsg::Shutdown).await;
    });
    let brain = Brain::new(BrainConfig::new(workspace), host, store, sinks);
    let report = brain.run(rx).await;
    drop(sweep_ticker);
    _lease_beat.abort();
    // Reap the watcher on shutdown rather than orphaning it.
    if let Ok(mut g) = watch_slot.lock() {
        let _ = g.take();
    }
    eprintln!(
        "flywheel doorbell: embedded loop exited (injects={} sheds={} degraded={})",
        report.stats.injects, report.stats.sheds, report.stats.degraded
    );
    Ok(())
}

/// In-process bus watch (zero fork): kinds mirror the CLI fallback filter.
fn spawn_bus_watch(
    cfg: &Config,
    project: &str,
    workspace: &str,
    tx: mpsc::Sender<BrainMsg>,
    fw: doorbell_rt::wake_brain::WatchForwarder,
) -> bool {
    (|| -> Option<()> {
        let _mail = crate::mail_shim::Mail::from_config(Some(&cfg.toron_mail_config()))
            .ok()
            .flatten()?;
        // Remote-daemon mail has no in-process mailbox (`mailbox()` panics
        // there): no bus to watch, so the caller falls back to the wake socket.
        let mb = crate::mail_shim::try_mailbox()?;
        let bus = crate::mail_shim::block_on(async {
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
        let mut sub = crate::mail_shim::block_on(bus.subscribe_trusted(filter)).ok()?;
        // GS062-ADV-4: stamp on hosting thread before nesting the reader.
        let mut stamps =
            doorbell_rt::watchbeat::spawn_stamp_threads(config::state_dir(), workspace.to_string());
        std::thread::spawn(move || {
            // GS-062: mirror child watch — touch_event on each Event; stoppable
            // 1s touch_heartbeat while the subscription lives (never conflate).
            loop {
                let ev_opt = crate::mail_shim::block_on(sub.next());
                match ev_opt {
                    Some(toron_core::bus::SubscriptionEvent::Event(e)) => {
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
                        // mcc9: the forwarder backpressures this reader thread
                        // on a full channel (a slow brain parks the sensor);
                        // only a wedged/closed brain drops the line (counted)
                        // and ends the watch with ONE Died — the wake-sock
                        // replacement resumes from the last accepted cursor.
                        // Stamp .event only once the brain has the line.
                        if let Ok(line) = serde_json::to_string(&v) {
                            if fw.forward(line).reached_brain() {
                                stamps.on_event();
                            } else {
                                stamps.stop();
                                doorbell_rt::msg::deliver_died_blocking(&tx);
                                break;
                            }
                        }
                    }
                    Some(toron_core::bus::SubscriptionEvent::Eose) => continue,
                    Some(toron_core::bus::SubscriptionEvent::Closed(_)) | None => {
                        stamps.stop();
                        doorbell_rt::msg::deliver_died_blocking(&tx);
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
        // Plain sync test: hosts construct on the main thread, OUTSIDE
        // any runtime — mail_shim::block_on panics nested inside one.
        // Each side runs SEQUENTIALLY under its own XDG state tree with
        // identical fixture files: sweep drains are destructive
        // (files deleted after read), so concurrent sides race.
        // Hermetic: NOTHING writes outside temp dirs. Live toron/herdr
        // CLIs may run read-only (whois/inbox/list on fixture pins fail
        // empty); the toron transport is remote-refused (production uses
        // remote sync HTTP; in-process block_ons inside a runtime).
        let _home = crate::config::test_state_home();
        let toron_dir = std::env::temp_dir().join(format!("parity-toron-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&toron_dir);
        std::fs::create_dir_all(&toron_dir).unwrap();
        // SAFETY: under ENV_LOCK via _home; restored below.
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
        // Heard presence for both pins (freshness uses the real clock;
        // both sides read the same file).
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

        // Identical destructive inputs per side (drains delete after read).
        fn write_fixture(state: &std::path::Path, key: &str, role_key: &str) {
            let agents = state.join("agents");
            std::fs::create_dir_all(&agents).unwrap();
            std::fs::write(
                agents.join(format!("{key}.json")),
                serde_json::json!([
                    {"pane_id": "pane-a", "name": "herdr-a", "mail_identity": "PinA"},
                    {"pane_id": "pane-b", "name": "herdr-b", "mail_identity": "PinB"}
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
        // removed by _home's Drop, TORON_* below.
        fn set_xdg(state: &std::path::Path) {
            unsafe {
                std::env::set_var("XDG_STATE_HOME", state);
            }
        }

        // One scripted Msg log; fixed payloads so only `ts`
        // differs between runs. Built twice (BrainMsg is not Clone).
        let inbox_row =
            |ts: &str| serde_json::json!({"importance": "urgent", "created_ts": ts, "tags": []});
        let mk_script = || {
            vec![
                BrainMsg::HerdrStatus {
                    pin: "PinA".into(),
                    status: "idle".into(),
                },
                BrainMsg::HerdrStatus {
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
                BrainMsg::HerdrStatus {
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
                // NOTE: no AckSlaTick in the live run (the embedded scan
                // block_ons inside a runtime); tick parity asserted above.
                // Busy presence holds the next routine ring on both hosts...
                BrainMsg::Presence {
                    pin: "PinA".into(),
                    state: "working".into(),
                },
                BrainMsg::HerdrStatus {
                    pin: "PinA".into(),
                    status: "working".into(),
                },
                BrainMsg::Watch(watch(9, serde_json::json!([["p", "PinA"]]), "e4", 4)),
                // ...while urgent breaks through on both.
                BrainMsg::Watch(watch(
                    14,
                    serde_json::json!([["p", "PinA"], ["toron-kind", "request-release"]]),
                    "e5",
                    5,
                )),
                // Drain under busy presence: routine audits, urgent prompts.
                BrainMsg::HerdrStatus {
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

        // Remote (refused) toron transport, both sides: fast-fails to the
        // hermetic CLI fallback (see module docs).
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

        // Side A: embedded host.
        let state_a = std::env::temp_dir().join(format!("parity-state-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_a);
        set_xdg(&state_a);
        write_fixture(&state_a, &key, &role_key);
        let dir_a = std::env::temp_dir().join(format!("parity-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_a);
        let (tx_a, rx_a) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        let rt_host = RtHost::new(remote_cfg(), &project, ws, tx_a.clone())
            .expect("rthost builds in sandbox");
        // Tick-level AckSla parity on throwaways (scan excluded live).
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
            sinks_a.add_sink("herdr", Box::new(MockSink::new().0));
            let brain_a = Brain::new(
                BrainConfig::new(ws),
                rt_host,
                doorbell_rt::FileStore::new(&dir_a),
                sinks_a,
            );
            run_script(brain_a, tx_a, rx_a, mk_script()).await
        });
        drop(rt);

        // Side B: daemon host (own state tree, same fixture content).
        let state_b = std::env::temp_dir().join(format!("parity-state-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_b);
        set_xdg(&state_b);
        write_fixture(&state_b, &key, &role_key);
        let dir_b = std::env::temp_dir().join(format!("parity-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_b);
        let (tx_b, rx_b) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        let mut fcfg = FileHostConfig::new(crate::config::state_dir(), project, ws.to_string());
        fcfg.toron_bin = "true".into();
        fcfg.herdr_bin = "true".into();
        let file_host = FileHost::new(fcfg).with_tx(tx_b.clone());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let rep_b = rt.block_on(async {
            let mut sinks_b = SinkManager::new(tx_b.clone());
            sinks_b.add_sink("herdr", Box::new(MockSink::new().0));
            let brain_b = Brain::new(
                BrainConfig::new(ws),
                file_host,
                doorbell_rt::FileStore::new(&dir_b),
                sinks_b,
            );
            run_script(brain_b, tx_b, rx_b, mk_script()).await
        });
        drop(rt);

        // Same scripted inputs → same delivery accounting.
        assert_eq!(rep_a.stats.injects, rep_b.stats.injects, "injects");
        assert_eq!(rep_a.stats.sheds, rep_b.stats.sheds, "sheds");
        assert_eq!(rep_a.stats.degraded, rep_b.stats.degraded, "degraded");
        assert_eq!(
            rep_a.stats.digest_batches, rep_b.stats.digest_batches,
            "digests"
        );
        // Journal diff empty modulo ts: the bd-ene2 verify.
        let a = read_journal(&dir_a, ws);
        let b = read_journal(&dir_b, ws);
        assert_eq!(a.len(), b.len(), "journal row counts differ");
        for (i, (ra, rb)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(ra, rb, "journal row {i} differs:\n{ra:?}\n{rb:?}");
        }
        unsafe {
            std::env::remove_var("TORON_DATA_DIR");
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
        // RestartWatch effect agrees tick-level (dispatch would spawn
        // watch children on both sides — covered by unit tests instead).
        let mut core = doorbell_rt::CoreReducer::default();
        let fx = core.tick_effects(&BrainMsg::Died);
        assert!(
            fx.iter()
                .any(|e| matches!(e, doorbell_rt::Effect::RestartWatch))
        );
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
mod off_runtime_tests {
    use super::*;
    use doorbell_rt::Host;

    /// Every mail-touching sync host method, called the way the brain calls
    /// them: from inside a running runtime, with IN-PROCESS mail. Inline,
    /// each of these panics "Cannot start a runtime from within a runtime".
    #[test]
    fn host_mail_methods_do_not_nest_block_on_inside_a_runtime() {
        let _home = crate::config::test_state_home();
        // In-process mail (no [mail.toron] url); built OUTSIDE any runtime.
        let _mail = crate::mail_shim::Mail::from_config(None)
            .expect("in-process mail")
            .expect("some");
        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), "offrt-methods", "offrt", tx).expect("host");
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            // maintenance sweep -> build_index (whois)
            let _ = host.sweep_maintenance();
            // ack-sla scan
            let _ = host.scan_ack_sla();
            // inbox read (`last` skips the timestamp parse, so the in-process
            // `inbox_filtered` `block_on` is actually reached)
            let _ = host.fetch_inbox_process("SomePin", "last");
            // tick -> live_decide_snapshot
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
        let _mail = crate::mail_shim::Mail::from_config(Some(&cfg.toron_mail_config()));
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
        let _mail = crate::mail_shim::Mail::from_config(None)
            .expect("in-process mail")
            .expect("some");
        let ws = "offrt-embedded";
        let cfg = Config::default();
        let h = std::thread::spawn(move || run_embedded(&cfg, "offrt-embedded", ws));
        std::thread::sleep(Duration::from_secs(4));
        let died_early = h.is_finished();
        // Stop the loop: it exits when its pid file goes away.
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

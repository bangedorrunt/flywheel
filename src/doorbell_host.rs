// governed-by: ADR-0012
//! Embedded host binding (bd-asr2.5): [`RtHost`] implements
//! `doorbell_rt::Host` over the live flywheel machinery — `workspace::tick`
//! as the decider, `handle_workspace_cmds` as the effect layer, the spawn
//! registry / mailbox / hc shims as producers. [`run_embedded`] wires
//! sensors around a `doorbell_rt::Brain` and runs it to shutdown.
//!
//! One implementation: every arm below mirrors `cmd_internal_doorbell`
//! (removed); behavioral deltas are marked DELTA with their reason.

#[path = "reservation_digest.rs"]
mod reservation_digest;

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

// ---- bd-l5vb2 U8: toron events as a wake source for no-door harnesses ----
// The PURE mapper/fold lives in `events_source.rs`; this file is the effect
// layer: journal before invoke, stream/poll transport, cursor persistence.

/// Journal row for an events-source effect. No payload/token ever rides it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EventsJournalOp {
    Subscribe,
    Fetch,
    Cursor(String),
}

fn events_journal_path(state_dir: &std::path::Path, pin: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("events-{pin}.jsonl"))
}

fn events_cursor_path(state_dir: &std::path::Path, pin: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("events-{pin}.cursor"))
}

fn read_events_cursor(state_dir: &std::path::Path, pin: &str) -> String {
    std::fs::read_to_string(events_cursor_path(state_dir, pin))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn write_events_cursor(state_dir: &std::path::Path, pin: &str, cursor: &str) {
    let path = events_cursor_path(state_dir, pin);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, cursor);
}

/// Journal-before-invoke: append the row BEFORE the RPC runs so a crash can
/// replay from the last cursor; the bus id deduper absorbs the duplicate.
fn append_events_journal(
    state_dir: &std::path::Path,
    pin: &str,
    op: &EventsJournalOp,
    now_us: i64,
) {
    use std::io::Write as _;
    let path = events_journal_path(state_dir, pin);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let row = match op {
        EventsJournalOp::Subscribe => {
            serde_json::json!({"op":"subscribe","pin":pin,"ts_us":now_us})
        }
        EventsJournalOp::Fetch => serde_json::json!({"op":"fetch","pin":pin,"ts_us":now_us}),
        EventsJournalOp::Cursor(c) => {
            serde_json::json!({"op":"cursor","pin":pin,"cursor":c,"ts_us":now_us})
        }
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{row}");
    }
}

struct FileEventsJournal<'a> {
    state_dir: &'a std::path::Path,
    pin: &'a str,
}

impl crate::events_source::PumpJournal for FileEventsJournal<'_> {
    fn row(&mut self, op: &str, cursor: Option<&str>) {
        let op = match op {
            "fetch" => EventsJournalOp::Fetch,
            "cursor" => {
                let c = cursor.unwrap_or("").to_string();
                write_events_cursor(self.state_dir, self.pin, &c);
                EventsJournalOp::Cursor(c)
            }
            other => EventsJournalOp::Cursor(other.to_string()),
        };
        append_events_journal(self.state_dir, self.pin, &op, now_us());
    }
}

struct BrainEventsSink<'a> {
    tx: &'a mpsc::Sender<BrainMsg>,
}

impl crate::events_source::PumpSink for BrainEventsSink<'_> {
    fn send(&mut self, msg: BrainMsg) {
        let _ = self.tx.blocking_send(msg);
    }
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Parse `toron inbox cursor` stdout into rumor-time micros (bd-yc6jf).
/// None on any shape failure — the caller rings anyway (fail-open), so a
/// broken read degrades to the old behavior, never to a dropped wake.
fn parse_toron_cursor(raw: &[u8]) -> Option<i64> {
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()?
        .get("cursor_us")?
        .as_i64()
}

/// Stop flag shared by every events pump; set on graceful shutdown.
static EVENTS_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One seat's pump. `subscribe` once, then stream with a poll fallback, fold
/// every page through the pure kernel, emit msgs BEFORE journaling the cursor.
fn run_events_pump(tx: mpsc::Sender<BrainMsg>, project: String, pin: String, url: String) {
    let state_dir = config::state_dir();
    let token_path = crate::events_tokens::token_path(&state_dir, &project, &pin);
    let name = format!("flywheel-doorbell-{project}-{pin}");
    let mut client = crate::events_client::ToronEventsClient::new(
        crate::events_client::UreqRpc::new(&url, &token_path.to_string_lossy()),
        &name,
        3_600,
    );
    let stopped = || EVENTS_STOP.load(std::sync::atomic::Ordering::Relaxed);
    while !stopped() {
        append_events_journal(&state_dir, &pin, &EventsJournalOp::Subscribe, now_us());
        match client.subscribe() {
            Ok(_) => break,
            Err(_) => std::thread::sleep(Duration::from_secs(5)),
        }
    }
    let mut cursor = read_events_cursor(&state_dir, &pin);
    let mut deduper = crate::events_bridge::Deduper::new(1024);
    let mut journal = FileEventsJournal {
        state_dir: &state_dir,
        pin: &pin,
    };
    let mut sink = BrainEventsSink { tx: &tx };
    while !stopped() {
        if crate::events_source::pump_once(
            &client,
            &mut cursor,
            &mut deduper,
            &pin,
            &mut journal,
            &mut sink,
        )
        .is_err()
        {
            let _ = tx.blocking_send(BrainMsg::Degraded {
                source: format!("events:{pin}"),
                reason: "events fetch failed; will retry".into(),
            });
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Arm one events pump per no-door seat that has a provisioned token. Opt-in
/// only: with the flag off this spawns nothing, so behaviour is byte-identical.
fn spawn_events_pumps(cfg: &Config, project: &str, tx: &mpsc::Sender<BrainMsg>) {
    if !cfg.doorbell_toron_events() {
        return;
    }
    let url = std::env::var("FLYWHEEL_TORON_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:18080".to_string());
    let state_dir = config::state_dir();
    EVENTS_STOP.store(false, std::sync::atomic::Ordering::Relaxed);
    let seats: Vec<(String, Option<String>, bool)> = state::load_spawn_registry(project)
        .into_iter()
        .map(|seat| {
            let pin = seat.mail_identity.clone();
            let has_token = pin.as_ref().is_some_and(|pin| {
                crate::events_tokens::token_path(&state_dir, project, pin).is_file()
            });
            (seat.harness.clone().unwrap_or_default(), pin, has_token)
        })
        .collect();
    for pin in crate::events_source::seats_to_pump(cfg.doorbell_toron_events(), &seats) {
        let tx = tx.clone();
        let project = project.to_string();
        let url = url.clone();
        let pin = pin.clone();
        std::thread::spawn(move || run_events_pump(tx, project, pin, url));
    }
}

/// Hook-reported turn state beats the plane's screen-derived status while it
/// is fresh; a stale/missing report leaves the status untouched.
fn apply_hook_turn_state(project: &str, updates: &mut [BrainMsg]) {
    let state_dir = config::state_dir();
    let now = crate::turn_hooks::now_us();
    for msg in updates.iter_mut() {
        if let BrainMsg::PlaneStatus { pin, status } = msg
            && let Some(turn) = crate::turn_hooks::read_turn(&state_dir, project, pin)
        {
            *status = crate::turn_state::status_from_turn(status, Some(&turn), now);
        }
    }
}

/// The embedded host: owns the reducer model and every flywheel-shaped
/// capability the brain orchestrates.
pub struct RtHost {
    cfg: Config,
    project: String,
    workspace: String,
    model: workspace::WorkspaceModel,
    generation: Option<doorbell::WatcherGen>,
    exe_path: PathBuf,
    brain_tx: mpsc::Sender<BrainMsg>,
    /// bd-0uzrq.17: the status-only lane. When set, the plane poll sends
    /// PlaneStatus here instead of the mail lane, so a status cannot queue
    /// behind a mail-bound dispatch (live: 76-155s).
    status_tx: Option<mpsc::Sender<BrainMsg>>,
    /// Panes with an active multiplexed events.subscribe forwarder.
    /// Shared with the reader thread so stream death can clear coverage and
    /// the next sweep re-arms (hc/tuios: stream death must not leave a
    /// pane stuck "covered" forever).
    covered_panes: Arc<Mutex<HashSet<String>>>,
    /// One rex presence poller per host (wave17-b): the boot seed is applied
    /// directly, and ongoing status changes ride a dedicated poll thread so
    /// no blocking plane call ever runs inside the brain's async tick.
    presence_armed: bool,
    /// Advisory reservation-conflict digest (bd-coordination-awareness):
    /// one notice per overlap episode, pushed to the holders and the captain.
    conflict_seen: std::collections::BTreeSet<String>,
    /// Pins already audited as holding a deferral with no observable pane
    /// (bd-346jl: a stuck wake must be visible, once per pin).
    deferred_audited: std::collections::BTreeSet<String>,
    /// bd-pbng7: the boot refold of journaled-but-unrung handoff rows runs
    /// once, on the first sweep, after the seed has told us which pins this
    /// host owns.
    boot_refolded: bool,
    conflict_checked_at: u64,
    pending_conflict_prompts: Vec<(String, String, Option<String>)>,
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
                if let Some(cursor) = fw.resume_cursor() {
                    doorbell_rt::wake_brain::write_cursor(&config::state_dir(), workspace, &cursor);
                }
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
    /// Deferrals whose pin has no pane this host can observe: a stuck wake
    /// must be visible, never a quiet queue (bd-346jl). One audit row per pin.
    fn stalled_deferrals(&mut self) -> Vec<Effect> {
        let queued: Vec<String> = self
            .model
            .doorbell
            .queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(pin, _)| pin.clone())
            .collect();
        if queued.is_empty() {
            return Vec::new();
        }
        let mapped: std::collections::HashSet<String> = state::load_spawn_registry(&self.project)
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect();
        queued
            .into_iter()
            .filter(|pin| !mapped.contains(pin))
            .filter(|pin| self.deferred_audited.insert(pin.clone()))
            .map(|pin| Effect::Audit {
                kind: "deferred_stalled".into(),
                pin,
                detail: serde_json::json!({
                    "why": "deferred wake has no pane this host observes; the at-prompt drain cannot fire",
                }),
            })
            .collect()
    }
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
        let rich_index = build_index(&cfg, project);
        merge_index(&mut model.doorbell.index, &rich_index);
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
            generation: None,
            exe_path,
            brain_tx,
            status_tx: None,
            covered_panes: Arc::new(Mutex::new(HashSet::new())),
            presence_armed: false,
            conflict_seen: std::collections::BTreeSet::new(),
            deferred_audited: std::collections::BTreeSet::new(),
            boot_refolded: false,
            conflict_checked_at: 0,
            pending_conflict_prompts: Vec::new(),
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
    ///
    /// The deferral trail also stays on the rt journal: `doorbell why` folds
    /// those rows to explain a wake that did not ring now, and an audit the
    /// journal never saw cannot be explained.
    fn run_durable_audit(&mut self, cmds: Vec<workspace::Cmd>) -> Vec<workspace::Cmd> {
        let mut rest = Vec::with_capacity(cmds.len());
        for cmd in cmds {
            let journaled = matches!(
                &cmd,
                workspace::Cmd::Audit { kind, .. }
                    if matches!(kind.as_str(), "deferred" | "drained" | "queued")
            );
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
            if journaled {
                rest.push(cmd);
            }
        }
        rest
    }

    /// Share the brain's death bell. Call before the first wake arms.
    pub fn set_died_bell(&mut self, bell: std::sync::Arc<doorbell_rt::DiedBell>) {
        self.died_bell = bell;
    }

    fn registry_pins(&self) -> Vec<String> {
        // Deduped: duplicate rows for one pane must not double the fetch.
        state::load_spawn_registry(&self.project)
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Registry pins whose pane is still on the plane (bd-3vkpe). Pure, so the
    /// fixture tests pin the rule: a retired/gone pin has no pane and no ring.
    pub(crate) fn live_pins_from(
        registry: &[state::SpawnEntry],
        live_panes: &[String],
    ) -> Vec<String> {
        let live: std::collections::BTreeSet<&String> = live_panes.iter().collect();
        registry
            .iter()
            .filter(|e| live.contains(&e.pane_id))
            .filter_map(|e| e.mail_identity.clone())
            .filter(|pin| !pin.is_empty())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Rex presence producer (wave17-b): the SAME source `flywheel status`
    /// trusts. Seeds one `agent_list()` (so a host that starts while panes
    /// are idle is injectable at once), then streams `rex events` scoped to
    /// the covered panes and folds every status change into a PlaneStatus.
    /// Pane ids the registry cannot map to a pin are audited loudly once —
    /// a covered pane is never silently skipped.
    /// bd-0uzrq.17: point the plane poll at the status lane.
    pub(crate) fn set_status_tx(&mut self, tx: mpsc::Sender<BrainMsg>) {
        self.status_tx = Some(tx);
    }

    /// The lane a plane status belongs on: its own when the entry point
    /// provided one, else the mail lane (tests and hosts without a runner).
    fn status_lane(&self) -> mpsc::Sender<BrainMsg> {
        match &self.status_tx {
            Some(tx) => tx.clone(),
            None => self.brain_tx.clone(),
        }
    }

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
        let mut pin_by_pane: HashMap<String, String> = pane_ids.iter().cloned().collect();
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
                let pane_ids: Vec<String> = agents.iter().map(|a| a.pane_id.clone()).collect();
                Self::extend_with_pane_bindings(
                    &self.cfg,
                    &self.project,
                    &mut pin_by_pane,
                    &pane_ids,
                );
                let (updates, unmapped) = Self::presence_updates_for(&agents, &pin_by_pane);
                let mut updates = updates;
                apply_hook_turn_state(&self.project, &mut updates);
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
        // bd-0uzrq.17: statuses get their own lane when the entry point
        // provided one; the mail lane stays for everything else.
        let tx = self.status_lane();
        let cfg = self.cfg.clone();
        let project = self.project.clone();
        std::thread::spawn(move || {
            // Transitions only: the brain channel is bounded and every repeated
            // row is one more stale status the fold must chew through. Live
            // (bd-0uzrq.17, 14:49-14:52Z) the unconditional republication kept
            // an idle pane reported `working` for minutes, so its queued wake
            // never drained.
            let mut last: HashMap<String, String> = HashMap::new();
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
                let mut pin_by_pane: HashMap<String, String> = state::load_spawn_registry(&project)
                    .iter()
                    .filter(|e| !e.pane_id.is_empty())
                    .filter_map(|e| e.mail_identity.clone().map(|pin| (e.pane_id.clone(), pin)))
                    .collect();
                let pane_ids: Vec<String> = agents.iter().map(|a| a.pane_id.clone()).collect();
                RtHost::extend_with_pane_bindings(&cfg, &project, &mut pin_by_pane, &pane_ids);
                let (mut updates, _) = RtHost::presence_updates_for(&agents, &pin_by_pane);
                apply_hook_turn_state(&project, &mut updates);
                for msg in RtHost::changed_statuses(&mut last, updates) {
                    if tx.blocking_send(msg).is_err() {
                        return;
                    }
                }
            }
        });
    }

    /// Keep only the statuses that differ from the last cycle's, recording the
    /// new ones. The plane source is already the one detector `block_state`;
    /// what the fold must not receive is the same answer again and again.
    fn changed_statuses(
        last: &mut HashMap<String, String>,
        updates: Vec<BrainMsg>,
    ) -> Vec<BrainMsg> {
        updates
            .into_iter()
            .filter(|msg| match msg {
                BrainMsg::PlaneStatus { pin, status } => match last.get(pin) {
                    Some(prev) if prev == status => false,
                    _ => {
                        last.insert(pin.clone(), status.clone());
                        true
                    }
                },
                _ => true,
            })
            .collect()
    }

    /// Advisory reservation-conflict digest: read the same board the reserve
    /// CLI exposes; on the FIRST sweep an overlap episode appears, queue one
    /// prompt per holder and one captain fact, and audit the episode. It is
    /// advisory by design: it never blocks a write, and the 180 s lease
    /// heartbeat frees a dead holder on its own.
    fn check_reservation_conflicts(&mut self, now_unix: u64) {
        let Ok(key) = crate::project_dir_str(&self.cfg, &self.project) else {
            return;
        };
        // The board through the SAME mail backend the host uses (remote MCP or
        // in-process). `try_mailbox()` is in-process-only and is None in the
        // remote mode the live host runs, which made the first digest silent.
        let Ok(Some(mail)) = crate::mail::Mail::from_config(Some(&self.cfg.toron_mail_config()))
        else {
            return;
        };
        // `list_active` blocks on the shared mail runtime in-process; the
        // sweep runs on the async host task, so the whole read goes off that
        // runtime (the off_runtime tests caught the nested block_on).
        let Ok(board) =
            crate::backend::run_mail_off_runtime_sync(|| mail.list_active(&key, None, None))
        else {
            return;
        };
        let rows: Vec<reservation_digest::Lease> = board
            .get("reservations")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        Some(reservation_digest::Lease {
                            holder: r.get("holder")?.as_str()?.to_string(),
                            path: r.get("path")?.as_str()?.to_string(),
                            exclusive: r.get("exclusive")?.as_bool()?,
                            expires_us: r.get("expires_ts_us").and_then(|v| v.as_i64()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let now_us = i64::try_from(now_unix)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000_000);
        let conflicts = reservation_digest::live_conflicts(&rows, now_us);
        let current: std::collections::BTreeSet<String> =
            conflicts.iter().map(|c| c.key.clone()).collect();
        for conflict in &conflicts {
            if !self.conflict_seen.insert(conflict.key.clone()) {
                continue;
            }
            for holder in [&conflict.holder_a, &conflict.holder_b] {
                self.pending_conflict_prompts.push((
                    holder.clone(),
                    conflict.line_for(holder),
                    None,
                ));
            }
            let summary = format!(
                "[reserve] {} and {} hold overlapping exclusive leases ({} / {}) — advisory: re-plan or request-release; leases self-free in 180 s",
                conflict.holder_a, conflict.holder_b, conflict.path_a, conflict.path_b
            );
            let _ = self.brain_tx.try_send(BrainMsg::Fact {
                summary: summary.clone(),
            });
            doorbell::audit_append(
                &config::state_dir(),
                &self.workspace,
                serde_json::json!({
                    "kind": "reservation_conflict",
                    "holders": [conflict.holder_a, conflict.holder_b],
                    "paths": [conflict.path_a, conflict.path_b],
                }),
            );
        }
        // Episode reset: an overlap that clears may ring again if it recurs.
        self.conflict_seen = current;
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

    /// Extend the registry's pane→pin map with the pane-identity bridge:
    /// `flywheel arrive` binds a pane (often a rex block in ANOTHER session)
    /// to a pin without a spawn-registry row, so a registry-only map reports
    /// that pane as unmapped and its at-prompt transition never fires — the
    /// deferred wake then never drains (bd-346jl). Pure over the resolver.
    fn bindings_for_unmapped(
        map: &HashMap<String, String>,
        pane_ids: &[String],
        resolve: impl Fn(&str) -> Option<String>,
    ) -> Vec<(String, String)> {
        pane_ids
            .iter()
            .filter(|pane| !map.contains_key(*pane))
            .filter_map(|pane| resolve(pane).map(|pin| (pane.clone(), pin)))
            .collect()
    }

    /// Live wrapper over [`Self::bindings_for_unmapped`]: same mail surface
    /// and project key the identity bridge was written with.
    fn extend_with_pane_bindings(
        cfg: &Config,
        project: &str,
        map: &mut HashMap<String, String>,
        pane_ids: &[String],
    ) {
        let Ok(key) = crate::project_dir_str(cfg, project) else {
            return;
        };
        let Ok(mail) = crate::cmd::mail::mail_or_die(cfg) else {
            return;
        };
        for (pane, pin) in Self::bindings_for_unmapped(map, pane_ids, |pane| {
            mail.resolve_pane_identity(&key, pane)
                .get("agent_name")
                .and_then(serde_json::Value::as_str)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
        }) {
            map.insert(pane, pin);
        }
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
        // The RESOLVED plane, not the configured name: `auto` is not an hc
        // verdict, and `auto` that really serves hc still needs coverage.
        if crate::backend::resolve_plane(&self.cfg).serving() != "hc" {
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

/// Union, never replace: the watcher must keep resolving every pin pane status
/// already taught it (a manual attach may never reach the spawn registry), and
/// the registry pass adds the npubs that kind-9 `p` tags carry.
fn merge_index(into: &mut doorbell::SelfIndex, from: &doorbell::SelfIndex) {
    for pin in from.pin_names() {
        into.insert_pin(pin.to_string(), from.npub(pin).map(str::to_string));
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// bd-tdpix: a LOST atomic claim emits no Prompt. Pure over the effect list so
/// the ordering (claim before ring) is witnessable without a live host.
#[cfg(test)]
pub(crate) fn gate_prompt_on_claim(claimed: bool, effects: Vec<Effect>) -> Vec<Effect> {
    if claimed {
        effects
    } else {
        effects
            .into_iter()
            .filter(|effect| !matches!(effect, Effect::Prompt { .. }))
            .collect()
    }
}

/// A Watch line spends a claim ONLY when this tick is about to emit that
/// line's Prompt. A deferred-but-gated line leaves the marker free for the
/// native path to ring; when the deferral later rings, the same line arrives
/// with a Prompt and claims then (bd-tdpix review).
#[cfg(test)]
pub(crate) fn claim_and_gate(
    state_dir: &std::path::Path,
    _line: &str,
    effects: Vec<Effect>,
) -> Vec<Effect> {
    claim_all_prompts(state_dir, effects)
}

/// Every Prompt that carries a bus event id claims before delivery —
/// immediate, deferred, stalled or boot-drained. A lost claim drops only that
/// Prompt; other effects and id-less Prompts pass through (bd-tdpix).
pub(crate) fn claim_all_prompts(state_dir: &std::path::Path, effects: Vec<Effect>) -> Vec<Effect> {
    effects
        .into_iter()
        .filter(|effect| match effect {
            Effect::Prompt {
                event_id: Some(id), ..
            } => crate::wake_rung::claim(state_dir, id),
            _ => true,
        })
        .collect()
}

fn cmds_to_effects(cmds: Vec<workspace::Cmd>) -> Vec<Effect> {
    cmds.into_iter()
        .filter_map(|c| match c {
            workspace::Cmd::Prompt {
                pin,
                line,
                event_id,
            } => Some(Effect::Prompt {
                // bd-ofa1s/bd-tdpix: the id is typed data carried from the
                // Watch line, never parsed from plain-language wake text.
                event_id,
                pin,
                line,
            }),
            workspace::Cmd::FetchInbox => Some(Effect::FetchInbox),
            workspace::Cmd::ReadToronCursor { pin } => Some(Effect::ReadToronCursor { pin }),
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

impl Host for RtHost {
    /// bd-3vkpe: only pins that still HAVE a pane can hear a ring. A gone or
    /// retired pin's row stays in the registry, so `pins()` is the wrong
    /// question for a wake — the plane holds the truth.
    /// bd-jhz8x.2: observe escalations go to the captain. Same fold the guard
    /// and `retire` use, never a name literal.
    fn captain_pin(&self) -> Option<String> {
        state::captain_pin(&self.project, &self.project)
    }

    fn live_pins(&self) -> Option<Vec<String>> {
        let registry = state::load_spawn_registry(&self.project);
        let backend = crate::backend::multiplexer_backend(&self.cfg).ok()?;
        // A plane we cannot read is not evidence that every pin is gone.
        let agents = backend.agent_list().ok()?;
        let panes: Vec<String> = agents.into_iter().map(|a| a.pane_id).collect();
        Some(Self::live_pins_from(&registry, &panes))
    }

    fn tick(&mut self, msg: &BrainMsg) -> Vec<Effect> {
        // bd-yc6jf: feed the pure fold the one notion of consumed — each
        // hosted pin's plane-P pull cursor from WakeState.since_by_pin, in
        // micros. A mail watch at or below it is already pulled and must not
        // ring an empty wake. Cheap local read; no toron round trip.
        let state_dir = config::state_dir();
        for entry in state::load_spawn_registry(&self.project) {
            let Some(pin) = entry.mail_identity else {
                continue;
            };
            if let Some(cursor) = crate::doorbell::pin_cursor_us(&state_dir, &self.workspace, &pin)
            {
                self.model.doorbell.pulled_us.insert(pin, cursor as i64);
            }
        }
        let mut boot_drain_effects: Vec<Effect> = Vec::new();
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
            BrainMsg::ToronCursor {
                pin,
                cursor_us,
                read_at_secs,
            } => (
                workspace::Msg::ToronCursor {
                    pin: pin.clone(),
                    cursor_us: *cursor_us,
                    read_at_secs: *read_at_secs,
                },
                false,
            ),
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
            BrainMsg::PromptRefused { pin, line, .. } => (
                workspace::Msg::PromptRefused {
                    pin: pin.clone(),
                    line: line.clone(),
                    event_id: None,
                },
                false,
            ),
            BrainMsg::EventsHint {
                event_id,
                pin,
                line,
            } => (
                workspace::Msg::EventsHint {
                    event_id: event_id.clone(),
                    pin: pin.clone(),
                    line: line.clone(),
                },
                false,
            ),
            BrainMsg::EventsGap { pin } => (workspace::Msg::EventsGap { pin: pin.clone() }, false),
            BrainMsg::Degraded { .. }
            | BrainMsg::Delivered { .. }
            | BrainMsg::Ask { .. }
            | BrainMsg::Shutdown => return Vec::new(),
        };
        if matches!(msg, BrainMsg::Sweep { .. }) {
            let hosted: std::collections::BTreeSet<String> =
                self.model.presence.keys().cloned().collect();
            let state_dir = crate::config::state_dir();
            if !self.boot_refolded {
                self.boot_refolded = true;
                let mut idle_pins: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for (pin, line) in crate::doorbell::unrung_deferrals_after(
                    &state_dir,
                    std::iter::once(self.workspace.clone()),
                    &|pin| crate::doorbell::pin_cursor_us(&state_dir, &self.workspace, pin),
                ) {
                    // bd-evuag: a pin already idle during the restart gap never
                    // produces the working->idle edge the drain relies on. Feed
                    // the deferral as PromptRefused (queues + status working),
                    // then synthesize ONE idle edge below so it rings through
                    // the normal budget and guard.
                    let idle_at_boot = matches!(
                        self.model.presence.get(&pin),
                        Some(workspace::Presence::Idle { .. })
                    );
                    let (next, cmds) = workspace::tick(
                        &self.model,
                        &workspace::Msg::PromptRefused {
                            pin: pin.clone(),
                            line: line.clone(),
                            event_id: None,
                        },
                    );
                    self.model = next;
                    if let Err(e) = crate::doorbell::journal_deferred_wake_with_wm(
                        &state_dir,
                        &self.workspace,
                        &pin,
                        &line,
                        &cmds,
                        Some(crate::doorbell::now_micros()),
                    ) {
                        eprintln!("flywheel: boot refold journal failed: {e}");
                    }
                    if idle_at_boot {
                        idle_pins.insert(pin);
                    }
                }
                for pin in idle_pins {
                    let (next, drain_cmds) = workspace::tick(
                        &self.model,
                        &workspace::Msg::PlaneStatus {
                            pin: pin.clone(),
                            status: "idle".into(),
                        },
                    );
                    self.model = next;
                    boot_drain_effects.extend(cmds_to_effects(drain_cmds));
                }
            }
            let mut journaled = true;
            for (pin, line, event_id) in crate::doorbell::take_deferred_wakes_after(
                &state_dir,
                &self.project,
                &hosted,
                &|pin| crate::doorbell::pin_cursor_us(&state_dir, &self.workspace, pin),
            ) {
                let (next, cmds) = workspace::tick(
                    &self.model,
                    &workspace::Msg::PromptRefused {
                        pin: pin.clone(),
                        line: line.clone(),
                        event_id: event_id.clone(),
                    },
                );
                self.model = next;
                if let Err(e) = crate::doorbell::journal_deferred_wake_with_wm(
                    &state_dir,
                    &self.workspace,
                    &pin,
                    &line,
                    &cmds,
                    Some(crate::doorbell::now_micros()),
                ) {
                    eprintln!("flywheel: deferred wake journal failed: {e}");
                    journaled = false;
                }
            }
            if journaled {
                crate::doorbell::commit_deferred_wakes(&state_dir, &self.project);
            }
        }
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
        // bd-6qae0: a held mail must reach the CAPTAIN as a plain-language
        // fact, not only as an audit row the human never reads.
        for cmd in &cmds {
            if let workspace::Cmd::Audit { kind, detail, .. } = cmd
                && crate::doorbell::should_notify_captain(kind)
            {
                let _ = self.brain_tx.try_send(BrainMsg::Fact {
                    summary: crate::doorbell::captain_fact_for_blocked(detail),
                });
            }
        }
        if let Some(recorder) = self.state_log.as_ref()
            && let Err(refusal) = recorder.enqueue(&self.model, &[])
        {
            eprintln!("flywheel: run-state boundary not recorded: {refusal}");
        }
        let mut effects = cmds_to_effects(cmds);
        // bd-tdpix: claim the shared bus-event marker BEFORE any Prompt is
        // emitted; a lost claim drops the Prompt from this tick. The boot drain
        // carries no event id and is appended after the gate.
        effects.extend(boot_drain_effects);
        if matches!(msg, BrainMsg::Sweep { .. }) {
            // bd-tdpix: a conflict Prompt can carry a bus event id, so it must
            // join the list BEFORE the claim or it bypasses the gate entirely.
            effects.extend(
                self.pending_conflict_prompts
                    .drain(..)
                    .map(|(pin, line, event_id)| Effect::Prompt {
                        // bd-tdpix: the id is typed data, never parsed from
                        // the plain-language wake text (bd-ofa1s).
                        event_id,
                        pin,
                        line,
                    }),
            );
        }
        // bd-tdpix: claim at delivery time for EVERY path, including deferred,
        // boot-drained and conflict Prompts that arrive outside the Watch arm.
        effects = claim_all_prompts(&config::state_dir(), effects);
        if matches!(msg, BrainMsg::Sweep { .. }) {
            // bd-tdpix: bound the shared claim directory on every sweep.
            let _ = crate::wake_rung::sweep(
                &config::state_dir(),
                crate::wake_rung::WAKE_RUNG_TTL_SECS,
                unix_now(),
            );
            effects.extend(self.stalled_deferrals());
        }
        effects
    }

    fn now_unix(&self) -> u64 {
        unix_now()
    }

    fn doorbell_index(&self) -> Option<&doorbell_core::SelfIndex> {
        Some(&self.model.doorbell.index)
    }

    /// GS-039 transition gate (audit P1): the reducer model owns the
    /// last-known status — `tick` ran `workspace::tick` before the brain
    /// reads this, so it must return the PREVIOUS tick's value. Untrained
    /// pins (not in the map) report None (unknown → transitioned).
    fn prev_status(&self, pin: &str) -> Option<String> {
        self.model.doorbell.statuses.get(pin).cloned()
    }

    /// The self index resolves the pin's npub (`whois` at build time), so a
    /// reply authored by that npub clears the unanswered ring. Without it
    /// the hear tracker treats the ring as npub-less and an agent that
    /// answers is still reported unanswered forever.
    fn pin_npub(&self, pin: &str) -> Option<String> {
        self.model.doorbell.index.npub(pin).map(str::to_string)
    }

    /// The pin's committed toron pull cursor (bd-yc6jf): fail-open None on
    /// any spawn, exit, or shape failure — the fold rings anyway and audits
    /// the miss, so a broken read degrades to the old behavior, never to
    /// a dropped wake.
    fn read_toron_cursor(&self, pin: &str) -> Option<i64> {
        let output = std::process::Command::new("toron")
            .args([
                "inbox",
                "cursor",
                "--project",
                &self.project,
                "--as",
                pin,
                "--json",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_toron_cursor(&output.stdout)
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
        // One pin per identity: `flywheel add` can leave a kind-name row and
        // the pin row for the SAME pane, which doubled every inbox fetch for
        // that pin (and its prompts) each cycle. BTreeSet keeps it sorted.
        registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn sweep_maintenance(&mut self) -> Vec<(String, String)> {
        let pins_now: Vec<String> = self.registry_pins();
        let now = unix_now();
        if pins_now != self.index_pins || now.saturating_sub(self.index_refreshed_at) >= 60 {
            let rich_index = build_index(&self.cfg, &self.project);
            merge_index(&mut self.model.doorbell.index, &rich_index);
            self.index_pins = pins_now;
            self.index_refreshed_at = now;
        }
        self.cover_new_panes();
        if now.saturating_sub(self.conflict_checked_at) >= 30 {
            self.conflict_checked_at = now;
            self.check_reservation_conflicts(now);
        }
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
            let resume_from = Some(doorbell_rt::wake_brain::start_cursor(
                resume.lock().ok().and_then(|g| g.clone()),
                unix_now(),
            ));
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
    let _ = doorbell::cleanup_legacy_sidecars(&config::state_dir());
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
    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
    // Always capture THIS host's binary. Reusing a gen left by a pre-install
    // spawn made the liveness check see a stale binary and exit-loop under
    // launchd (the host itself is the current binary).
    let generation = doorbell::WatcherGen::capture(&exe_path, unix_now());
    doorbell::write_state(
        &doorbell::state_file(&config::state_dir(), workspace),
        &doorbell::DoorbellState {
            pid: Some(std::process::id()),
            project: Some(project.to_string()),
            generation: generation.clone(),
        },
    )?;
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
    // bd-0uzrq.17: plane statuses ride their own lane, polled by the brain
    // before mail, so a status cannot queue behind a mail-bound dispatch.
    host.set_status_tx(runner.status_sender());
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
    // ADR-0041: ONE wake door. The comms envelope host rings while comms is
    // enabled; the NIP-17 mail wake is the door only when it is not, because
    // the one-turn peer handler stops when a pane finishes and a `done` pane
    // still needs a DM wake.
    let _mail_wake =
        crate::mail_wake::mail_wake_owns(crate::comms_send::enabled(&cfg)).then(|| {
            crate::mail_wake::MailWakeHost::spawn(
                cfg.clone(),
                project.to_string(),
                workspace.to_string(),
            )
        });
    let store = FileStore::new(config::state_dir());
    // bd-l5vb2: arm the opt-in toron events source alongside wake.sock.
    spawn_events_pumps(&cfg, workspace, &tx);
    let watch_slot = host.wake_task.clone();
    let bus_fw = wake_forwarder(
        &tx,
        workspace,
        host.wake_counters.clone(),
        host.wake_resume.clone(),
    );
    let bus_armed = if arm_in_process_bus(&doorbell_rt::wake_sock::wake_sock_path()) {
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
    } else {
        false
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
            Some(doorbell_rt::wake_brain::start_cursor(
                doorbell_rt::wake_brain::read_cursor(&config::state_dir(), workspace),
                unix_now(),
            )),
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
    let state_path = doorbell::state_file(&config::state_dir(), workspace);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
    let probe_cfg = cfg.clone();
    let probe_project = project.to_string();
    let probe_ws = workspace.to_string();
    let probe_dir = config::state_dir();
    let shutdown_tx = tx.clone();
    let liveness_workspace = workspace.to_string();
    // wave15-b: only a stale binary restarts the host. The flag rides out
    // of the liveness task; `run_embedded_async` turns it into a
    // restartable (nonzero) exit instead of the silent exit 0 that used to
    // strand supervised hosts on the old binary. Deliberate retires (pid
    // gone, workspace gone, daemon stop) stay exit 0 and are never
    // restarted.
    let stale_binary = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stale_binary_cb = std::sync::Arc::clone(&stale_binary);
    let workspace_gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let workspace_gone_cb = std::sync::Arc::clone(&workspace_gone);
    let _liveness = tokio::spawn(async move {
        let mut gone_streak = 0u32;
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if !state_path.exists() {
                break;
            }
            if let Some(record) =
                doorbell::dispatcher_generation(&config::state_dir(), &liveness_workspace)
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
                    workspace_gone_cb.store(true, std::sync::atomic::Ordering::SeqCst);
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
    if workspace_gone.load(std::sync::atomic::Ordering::SeqCst) {
        doorbell::retire_workspace(&config::state_dir(), workspace, std::process::id());
    }
    _lease_beat.abort();
    EVENTS_STOP.store(true, std::sync::atomic::Ordering::Relaxed);
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

/// The in-process bus only fans out events published by THIS process. A toron
/// daemon publishes mail in its own process and streams it through the wake
/// socket, so arming the bus while that socket exists would shadow the only
/// live source with a subscription that never sees a mail event.
fn arm_in_process_bus(wake_socket: &std::path::Path) -> bool {
    !wake_socket.exists()
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
mod toron_cursor_parse_tests {
    //! bd-yc6jf: the cursor parse is total — any shape failure is None
    //! (fail-open: ring anyway), never a panic, never a wrong number.

    #[test]
    fn valid_cursor_parses_to_micros() {
        assert_eq!(
            super::parse_toron_cursor(
                br#"{"project":"flywheel","agent":"BlueRidge","cursor_us":1700000000000000}"#
            ),
            Some(1_700_000_000_000_000)
        );
    }

    #[test]
    fn any_shape_failure_is_none() {
        for raw in [
            &b""[..],
            b"not json",
            br#"{"cursor_us": "a lot"}"#,
            br#"{"cursor_us": 1.5}"#,
            br#"{"other": 1}"#,
        ] {
            assert_eq!(super::parse_toron_cursor(raw), None, "fail-open");
        }
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
    fn a_real_watch_line_through_the_fold_carries_and_claims_its_event_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let line = serde_json::json!({
            "kind": 9,
            "id": "bus-fold-42",
            "tags": [["p", "PinA"], ["toron-kind", "ack-sla-expired"], ["mid", "msg-fold-42"]],
            "content": "sealed"
        })
        .to_string();
        let mut model = crate::workspace::WorkspaceModel::default();
        model.doorbell.index.insert_pin("PinA".into(), None);
        model.roles.insert("captain".into(), "PinA".into());
        let (mut model, _) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::PlaneStatus {
                pin: "PinA".into(),
                status: "idle".into(),
            },
        );
        model
            .presence
            .insert("PinA".into(), crate::workspace::Presence::from_str("idle"));
        let (_, cmds) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson { line: line.clone() },
        );
        let effects = super::cmds_to_effects(cmds);
        let carried = effects.iter().find_map(|effect| match effect {
            Effect::Prompt { event_id, .. } => event_id.clone(),
            _ => None,
        });
        assert_eq!(
            carried.as_deref(),
            Some("bus-fold-42"),
            "the fold carries the bus id; effects={effects:?}"
        );
        // Native already claimed: the doorbell must stay silent for this event.
        assert!(crate::wake_rung::claim(dir.path(), "bus-fold-42"));
        let silent = super::claim_all_prompts(dir.path(), effects.clone());
        assert!(!silent.iter().any(|e| matches!(e, Effect::Prompt { .. })));
        // With a free marker, the same folded effects ring exactly once.
        let free = tempfile::tempdir().expect("tempdir");
        let rang = super::claim_all_prompts(free.path(), effects);
        assert_eq!(
            rang.iter()
                .filter(|e| matches!(e, Effect::Prompt { .. }))
                .count(),
            1,
            "one event -> one ring"
        );
    }

    #[test]
    fn an_interleaved_b_watch_does_not_restamp_deferred_a() {
        // bd-ofa1s review: a wake deferred while the seat works must keep ITS
        // id even when another event's Watch arrives before the drain.
        let watch = |id: &str| {
            serde_json::json!({
                "kind": 9, "id": id,
                "tags": [["p", "PinA"], ["toron-kind", "mail-delivered"], ["mid", format!("m-{id}")]],
                "content": "sealed"
            })
            .to_string()
        };
        let mut model = crate::workspace::WorkspaceModel::default();
        model.doorbell.index.insert_pin("PinA".into(), None);
        model
            .doorbell
            .statuses
            .insert("PinA".into(), "working".into());
        model.presence.insert(
            "PinA".into(),
            crate::workspace::Presence::from_str("working"),
        );
        // A is deferred while PinA works.
        let (model, _) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson {
                line: watch("evt-A"),
            },
        );
        // B's Watch arrives while A is still queued.
        let (model, _) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson {
                line: watch("evt-B"),
            },
        );
        assert!(
            model
                .doorbell
                .queues
                .get("PinA")
                .map(|q| q.iter().any(|w| w.event_id.as_deref() == Some("evt-A")))
                .unwrap_or(false),
            "A keeps its own id in the queue after B arrived"
        );
        // Seat goes idle: the deferred A drains with A's id, never B's.
        let (_, cmds) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::PlaneStatus {
                pin: "PinA".into(),
                status: "idle".into(),
            },
        );
        let ids: Vec<Option<String>> = super::cmds_to_effects(cmds)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Prompt { event_id, .. } => Some(event_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec![Some("evt-A".into())],
            "A drains with A's id: {ids:?}"
        );
        // B's own Watch at idle rings with B's id.
        let (_, cmds) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson {
                line: watch("evt-B"),
            },
        );
        let ids: Vec<Option<String>> = super::cmds_to_effects(cmds)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Prompt { event_id, .. } => Some(event_id),
                _ => None,
            })
            .collect();
        // B folded into the same queue class; the invariant is that A's ring
        // kept A's id and was never restamped with B's.
        assert!(ids.iter().all(|id| id.as_deref() != Some("evt-B")));
    }

    #[test]
    fn a_deferred_ring_carries_the_id_through_the_queue_drain_and_claims() {
        // bd-ofa1s: a wake deferred while the seat is working must carry its
        // event id through the in-memory queue to the drain at idle, so the
        // claim gate sees it (free marker rings once; taken marker stays quiet).
        let line = serde_json::json!({
            "kind": 9,
            "id": "evt-def-claim",
            "tags": [["p", "PinA"], ["toron-kind", "mail-delivered"], ["mid", "msg-def"]],
            "content": "sealed"
        })
        .to_string();
        let mut model = crate::workspace::WorkspaceModel::default();
        model.doorbell.index.insert_pin("PinA".into(), None);
        model
            .doorbell
            .statuses
            .insert("PinA".into(), "working".into());
        model.presence.insert(
            "PinA".into(),
            crate::workspace::Presence::from_str("working"),
        );
        // Working seat: the wake is held in the queue with its typed id.
        let (mut model, _) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson { line: line.clone() },
        );
        let held = model
            .doorbell
            .queues
            .get("PinA")
            .expect("the working seat holds the wake");
        assert!(
            held.iter()
                .any(|w| w.event_id.as_deref() == Some("evt-def-claim")),
            "the typed id rides in the queue entry: {held:?}"
        );
        // Idle: the queued wake drains into a Prompt carrying the same id.
        model
            .presence
            .insert("PinA".into(), crate::workspace::Presence::from_str("idle"));
        let (_, cmds) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::PlaneStatus {
                pin: "PinA".into(),
                status: "idle".into(),
            },
        );
        let effects = super::cmds_to_effects(cmds);
        let carried = effects.iter().find_map(|e| match e {
            Effect::Prompt { event_id, .. } => event_id.clone(),
            _ => None,
        });
        assert_eq!(
            carried.as_deref(),
            Some("evt-def-claim"),
            "the drain carries the typed id: {effects:?}"
        );
        let free = tempfile::tempdir().expect("tempdir");
        let rang = super::claim_all_prompts(free.path(), effects.clone());
        assert_eq!(
            rang.iter()
                .filter(|e| matches!(e, Effect::Prompt { .. }))
                .count(),
            1,
            "a free marker rings exactly once"
        );
        let taken = tempfile::tempdir().expect("tempdir");
        assert!(crate::wake_rung::claim(taken.path(), "evt-def-claim"));
        let silent = super::claim_all_prompts(taken.path(), effects);
        assert!(
            !silent.iter().any(|e| matches!(e, Effect::Prompt { .. })),
            "a marker the native path already claimed stays silent"
        );
    }

    #[test]
    fn a_watch_line_without_a_prompt_leaves_the_claim_free() {
        let dir = tempfile::tempdir().expect("tempdir");
        let line =
            doorbell_core::doorbell_line(doorbell_core::EventClass::PlanePMail, "toron next");
        let effects = vec![Effect::FetchInbox];
        let kept = super::claim_and_gate(dir.path(), &line, effects);
        assert!(!kept.iter().any(|e| matches!(e, Effect::Prompt { .. })));
        assert!(
            crate::wake_rung::claim(dir.path(), "evt-gated"),
            "a deferred/gated line must not spend the marker"
        );
    }

    #[test]
    fn a_deferred_wake_claims_when_it_rings_and_is_silent_if_native_won() {
        let dir = tempfile::tempdir().expect("tempdir");
        let line =
            doorbell_core::doorbell_line(doorbell_core::EventClass::PlanePMail, "toron next");
        let effects = vec![Effect::Prompt {
            // bd-ofa1s: typed id, never parsed from the plain-language line.
            event_id: Some("evt-deferred".into()),
            pin: "ScarletCove".into(),
            line: line.clone(),
        }];
        // The native path claimed first: the deferred ring must stay silent.
        assert!(crate::wake_rung::claim(dir.path(), "evt-deferred"));
        let kept = super::claim_and_gate(dir.path(), &line, effects.clone());
        assert!(!kept.iter().any(|e| matches!(e, Effect::Prompt { .. })));
        // With the marker free, the same deferred ring wins and emits Prompt.
        let other = tempfile::tempdir().expect("tempdir");
        let won = super::claim_and_gate(other.path(), &line, effects);
        assert!(won.iter().any(|e| matches!(e, Effect::Prompt { .. })));
    }

    #[test]
    fn a_lost_claim_emits_no_prompt() {
        let effects = vec![
            Effect::Prompt {
                event_id: None,
                pin: "ScarletCove".into(),
                line: "wake".into(),
            },
            Effect::FetchInbox,
        ];
        let kept = super::gate_prompt_on_claim(false, effects.clone());
        assert!(!kept.iter().any(|e| matches!(e, Effect::Prompt { .. })));
        assert!(kept.iter().any(|e| matches!(e, Effect::FetchInbox)));
        let won = super::gate_prompt_on_claim(true, effects);
        assert_eq!(won.len(), 2, "a won claim keeps the Prompt");
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

    /// bd-0uzrq.17: statuses ride their own lane when the entry point
    /// provides one. The poll must not put them on the mail lane, or a
    /// mail-bound dispatch ahead of them delays every idle edge — live
    /// 16:16-16:19Z that was 76-155s between a rex record change and the
    /// journaled PlaneStatus, and the deferred wake waited with it.
    #[test]
    fn a_plane_status_rides_the_status_lane_when_one_is_set() {
        let (mail_tx, mut mail_rx) = mpsc::channel(8);
        let (status_tx, mut status_rx) = mpsc::channel(8);
        let mut host =
            RtHost::new(Config::default(), "lane-routing", "lane-routing", mail_tx).expect("host");
        host.set_status_tx(status_tx);

        let updates =
            RtHost::presence_updates_for(&[row("pane-idle", AgentStatus::Idle)], &pin_map()).0;
        assert_eq!(updates.len(), 1, "one mapped pin: {updates:?}");
        // The same send the poll thread performs, through the same choice.
        let tx = host.status_lane();
        for msg in updates {
            tx.try_send(msg).expect("status lane accepts");
        }
        assert!(
            status_rx.try_recv().is_ok(),
            "the status landed on its own lane"
        );
        assert!(
            mail_rx.try_recv().is_err(),
            "and NOT on the mail lane, where it would queue behind mail"
        );
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

    /// bd-0uzrq.17: the poll used to forward every pin every cycle, so a
    /// bounded brain channel carried a backlog of stale statuses — an idle pane
    /// was rung as `working` for minutes and its deferred wake never drained
    /// (live 14:49-14:52Z). Transitions only: the first answer, the idle flip,
    /// and nothing in between. The drain on that one idle flip is fenced by
    /// `a_busy_crew_prompt_rings_once_at_the_next_prompt`.
    #[test]
    fn only_a_status_change_reaches_the_brain() {
        let mut last = HashMap::new();
        let status =
            |st: AgentStatus| RtHost::presence_updates_for(&[row("pane-idle", st)], &pin_map()).0;

        let first = RtHost::changed_statuses(&mut last, status(AgentStatus::Working));
        assert_eq!(first.len(), 1, "the first answer is news: {first:?}");

        for _ in 0..3 {
            let repeat = RtHost::changed_statuses(&mut last, status(AgentStatus::Working));
            assert!(
                repeat.is_empty(),
                "a repeated working row is a stale row: {repeat:?}"
            );
        }

        let idle = RtHost::changed_statuses(&mut last, status(AgentStatus::Idle));
        assert_eq!(idle.len(), 1, "the idle flip must reach the fold: {idle:?}");
        match &idle[0] {
            BrainMsg::PlaneStatus { pin, status } => {
                assert_eq!(pin, "PinA");
                assert_eq!(status, "idle");
                assert!(
                    doorbell_core::should_inject(status),
                    "an idle transition that is not injectable never drains the queue"
                );
            }
            other => panic!("expected PlaneStatus, got {other:?}"),
        }
        assert!(
            RtHost::changed_statuses(&mut last, status(AgentStatus::Idle)).is_empty(),
            "the idle itself does not repeat either"
        );

        // …and that ONE idle transition is what drains a wake queued while the
        // pane was working. The producer and the reducer are judged together:
        // a dedup that swallows the idle leaves the queue parked (bd-0uzrq.17).
        let mut model = crate::workspace::WorkspaceModel::default();
        model.doorbell.index.insert_pin("PinA".into(), None);
        model
            .doorbell
            .statuses
            .insert("PinA".into(), "working".into());
        let line = r#"{"kind":9,"id":"ev-17","tags":[["p","PinA"],["toron-kind","mail-delivered"],["mid","msg-17"]],"content":"sealed"}"#;
        let (queued, qcmds) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson {
                line: line.to_string(),
            },
        );
        assert!(
            !qcmds
                .iter()
                .any(|c| matches!(c, crate::workspace::Cmd::Prompt { .. })),
            "a working pane queues, never prompts: {qcmds:?}"
        );
        let BrainMsg::PlaneStatus { pin, status } = &idle[0] else {
            unreachable!()
        };
        let (_, drained) = crate::workspace::tick(
            &queued,
            &crate::workspace::Msg::PlaneStatus {
                pin: pin.clone(),
                status: status.clone(),
            },
        );
        assert!(
            drained
                .iter()
                .any(|c| matches!(c, crate::workspace::Cmd::Prompt { pin, .. } if pin == "PinA")),
            "the one idle transition drains the queued wake: {drained:?}"
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

/// bd-3vkpe: the fixture that reproduces the digest conflict — six gone pins
/// and one live one. Only the live pin is a wake target, so only it can ever be
/// "unanswered".
#[cfg(test)]
mod live_pins_tests {
    use super::RtHost;
    use crate::state::SpawnEntry;

    fn row(pane: &str, pin: &str) -> SpawnEntry {
        SpawnEntry::with_mail_identity(pane, format!("pane-{pane}"), pin)
    }

    #[test]
    fn a_gone_pins_pane_is_not_a_wake_target() {
        let registry = vec![
            row("block:01a1e1c0-a", "SunnyMarsh"),
            row("block:01a1e1c0-b", "BoldBay"),
            row("block:01a1e1c0-c", "MistyMarsh"),
            row("block:01a1e1c0-d", "CyanCove"),
        ];
        // The plane holds exactly one of those panes.
        let live = RtHost::live_pins_from(&registry, &["block:01a1e1c0-d".to_string()]);
        assert_eq!(live, vec!["CyanCove".to_string()]);

        // Nothing live: every pin resolves as "no pane, nothing to ring" —
        // which is what the brain then does with its rings.
        assert!(
            RtHost::live_pins_from(&registry, &[]).is_empty(),
            "no pane means no wake target"
        );
    }

    #[test]
    fn a_retired_row_is_not_a_wake_target_even_if_its_pane_is_listed() {
        // A retired pin's row is gone; a stale plane row alone does not make it
        // addressable.
        let registry = vec![row("block:01a1e1c0-a", "SunnyMarsh")];
        let live = RtHost::live_pins_from(&registry, &["block:01a1e1c0-f".to_string()]);
        assert!(live.is_empty(), "{live:?}");
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

    /// bd-fwt8r: a ring queued before the recipient pulled the mail must be
    /// dropped at the idle transition, with a durable already-pulled audit.
    #[test]
    fn a_pulled_deferral_does_not_ring_at_idle() {
        let _home = crate::config::test_state_home();
        let project = format!("pulled-host-{}", std::process::id());
        let workspace = format!("session:pulled-{}", std::process::id());
        let state_dir = crate::config::state_dir();
        let pin = "BlueRidge";
        let wm = 1_700_000_000_000_000u64;
        crate::doorbell::append_deferred_wake_with_wm(
            &state_dir,
            &project,
            pin,
            &crate::doorbell::deferred_wake_line(),
            Some(wm),
        )
        .expect("queue the deferral");
        // The recipient pulled mail AFTER the ring was queued.
        let mut st = crate::doorbell::WakeState::default();
        st.since_by_pin
            .insert(pin.to_string(), "2023-11-14T22:14:00.000000Z".to_string());
        crate::doorbell::save_wake_state(
            &crate::doorbell::wake_state_file(&state_dir, &workspace),
            &st,
        )
        .expect("save pulled cursor");

        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), &project, &workspace, tx).expect("host");
        host.tick(&BrainMsg::PlaneStatus {
            pin: pin.to_string(),
            status: "working".into(),
        });
        host.tick(&BrainMsg::Sweep { now: 1 });
        let effects = host.tick(&BrainMsg::PlaneStatus {
            pin: pin.to_string(),
            status: "idle".into(),
        });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Prompt { pin: p, .. } if p == pin)),
            "a pulled ring must not prompt: {effects:?}"
        );
        let audit = state_dir.join(format!(
            "doorbell-already-pulled-{}.jsonl",
            crate::state::state_key(&project)
        ));
        let raw = std::fs::read_to_string(audit).expect("already-pulled journal");
        assert!(raw.contains(pin) && raw.contains("already-pulled"), "{raw}");
    }

    /// bd-tdpix follow-up: a conflict Prompt raised during a Sweep must claim
    /// its event before delivery, not after — the native path may have won.
    #[test]
    fn a_sweep_conflict_prompt_claims_before_delivery() {
        let _home = crate::config::test_state_home();
        let project = format!("sweep-conflict-{}", std::process::id());
        let workspace = format!("session:sweep-conflict-{}", std::process::id());
        let state_dir = crate::config::state_dir();
        let claimed =
            doorbell_core::doorbell_line(doorbell_core::EventClass::RequestRelease, "toron next");
        let free =
            doorbell_core::doorbell_line(doorbell_core::EventClass::RequestRelease, "toron next");
        assert!(crate::wake_rung::claim(&state_dir, "evt-conflict-claimed"));
        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), &project, &workspace, tx).expect("host");
        host.pending_conflict_prompts.push((
            "BlueRidge".into(),
            claimed,
            Some("evt-conflict-claimed".into()),
        ));
        host.pending_conflict_prompts.push((
            "LilacChapel".into(),
            free,
            Some("evt-conflict-free".into()),
        ));
        let effects = host.tick(&BrainMsg::Sweep { now: 1 });
        let prompt_ids: Vec<Option<String>> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Prompt { event_id, .. } => Some(event_id.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !prompt_ids
                .iter()
                .any(|id| id.as_deref() == Some("evt-conflict-claimed")),
            "native already claimed this conflict event: {prompt_ids:?}"
        );
        assert!(
            prompt_ids
                .iter()
                .any(|id| id.as_deref() == Some("evt-conflict-free")),
            "a free conflict marker must still ring: {prompt_ids:?}"
        );
    }

    /// The converse: a deferral whose watermark is still ahead of the pin's
    /// cursor must ring at idle, as before (bd-fwt8r).
    #[test]
    fn an_unpulled_deferral_still_rings_at_idle() {
        let _home = crate::config::test_state_home();
        let project = format!("unpulled-host-{}", std::process::id());
        let workspace = format!("session:unpulled-{}", std::process::id());
        let state_dir = crate::config::state_dir();
        let pin = "LilacChapel";
        let future_wm = u64::MAX;
        crate::doorbell::append_deferred_wake_with_wm(
            &state_dir,
            &project,
            pin,
            &crate::doorbell::deferred_wake_line(),
            Some(future_wm),
        )
        .expect("queue the deferral");
        let mut st = crate::doorbell::WakeState::default();
        st.since_by_pin
            .insert(pin.to_string(), "2023-11-14T22:14:00.000000Z".to_string());
        crate::doorbell::save_wake_state(
            &crate::doorbell::wake_state_file(&state_dir, &workspace),
            &st,
        )
        .expect("save cursor");

        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), &project, &workspace, tx).expect("host");
        host.tick(&BrainMsg::PlaneStatus {
            pin: pin.to_string(),
            status: "working".into(),
        });
        host.tick(&BrainMsg::Sweep { now: 1 });
        let effects = host.tick(&BrainMsg::PlaneStatus {
            pin: pin.to_string(),
            status: "idle".into(),
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Prompt { pin: p, .. } if p == pin)),
            "an unpulled deferral must still ring: {effects:?}"
        );
    }

    /// The CLI knows the project, never the dispatcher's plane session id, so
    /// the handoff is project-keyed; the host takes only the pins it hosts and
    /// folds the wake into its one queue, then rings it on the prompt
    /// transition.
    #[test]
    fn a_deferred_crew_wake_is_folded_and_rung_at_the_next_prompt() {
        let _home = crate::config::test_state_home();
        let project = format!("defer-host-{}", std::process::id());
        crate::doorbell::append_deferred_wake(
            &crate::config::state_dir(),
            &project,
            "BlueRidge",
            &crate::doorbell::deferred_wake_line(),
        )
        .expect("record the deferral");
        let (tx, _rx) = mpsc::channel(8);
        let mut host =
            RtHost::new(Config::default(), &project, "session:01a1d257", tx).expect("host");
        // A status is what makes this host the owner of the pin (presence is
        // the same set `cover_rex_panes` seeds for its own workspace).
        host.tick(&BrainMsg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "working".into(),
        });

        let effects = host.tick(&BrainMsg::Sweep { now: 1 });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Prompt { .. })),
            "a working pin is never prompted: {effects:?}"
        );
        let journal = std::fs::read_to_string(
            crate::config::state_dir()
                .join("journals")
                .join("doorbell-journal-session:01a1d257.jsonl"),
        )
        .expect("the taken wake is journaled");
        assert!(
            journal.contains("\"handoff\"")
                && journal.contains("PromptRefused")
                && journal.contains("\"deferred\""),
            "the handoff row carries its own fold actions: {journal}"
        );

        let effects = host.tick(&BrainMsg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Audit { kind, .. } if kind == "drained")),
            "the prompt transition drains the queue: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Prompt { pin, .. } if pin == "BlueRidge")),
            "and rings the deferred line: {effects:?}"
        );
    }

    /// One dispatcher per plane session (chiebukuro runs several). A host that
    /// does not host the pin must leave the handoff for the host that does;
    /// otherwise the wake is queued for a pane that host never prompts.
    #[test]
    fn a_foreign_pin_wake_is_left_for_the_host_that_owns_it() {
        let _home = crate::config::test_state_home();
        let project = format!("defer-two-{}", std::process::id());
        crate::doorbell::append_deferred_wake(
            &crate::config::state_dir(),
            &project,
            "BlueRidge",
            &crate::doorbell::deferred_wake_line(),
        )
        .expect("record the deferral");
        let (tx_a, _rx_a) = mpsc::channel(8);
        let mut host_a =
            RtHost::new(Config::default(), &project, "session:a", tx_a).expect("host a");
        host_a.tick(&BrainMsg::PlaneStatus {
            pin: "LilacChapel".into(),
            status: "working".into(),
        });
        let (tx_b, _rx_b) = mpsc::channel(8);
        let mut host_b =
            RtHost::new(Config::default(), &project, "session:b", tx_b).expect("host b");
        host_b.tick(&BrainMsg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "working".into(),
        });

        host_a.tick(&BrainMsg::Sweep { now: 1 });
        let journal_a = std::fs::read_to_string(
            crate::config::state_dir()
                .join("journals")
                .join("doorbell-journal-session:a.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            !journal_a.contains("handoff"),
            "the foreign host takes nothing: {journal_a}"
        );
        host_b.tick(&BrainMsg::Sweep { now: 1 });
        let journal_b = std::fs::read_to_string(
            crate::config::state_dir()
                .join("journals")
                .join("doorbell-journal-session:b.jsonl"),
        )
        .expect("the owning host journals the handoff");
        assert!(
            journal_b.contains("handoff") && journal_b.contains("BlueRidge"),
            "the owning host takes and journals it: {journal_b}"
        );
        let ring = host_b.tick(&BrainMsg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        });
        assert!(
            ring.iter()
                .any(|e| matches!(e, Effect::Prompt { pin, .. } if pin == "BlueRidge")),
            "and rings it on the prompt transition: {ring:?}"
        );
    }

    /// A dispatcher restart empties the per-pin queue. Boot must re-derive it
    /// from the journal, or a wake taken before the restart never rings.
    #[test]
    fn a_restart_refolds_a_journaled_unrung_handoff() {
        let _home = crate::config::test_state_home();
        let project = format!("refold-{}", std::process::id());
        let workspace = "session:refold";
        crate::doorbell::journal_deferred_wake(
            &crate::config::state_dir(),
            workspace,
            "BlueRidge",
            &crate::doorbell::deferred_wake_line(),
            &[],
        )
        .expect("journal the handoff");
        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), &project, workspace, tx).expect("host");
        host.tick(&BrainMsg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        });

        // bd-evuag: the pin was already idle at boot, so the refolded wake must
        // ring AT the boot sweep — no later working->idle edge will arrive.
        let effects = host.tick(&BrainMsg::Sweep { now: 1 });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Prompt { pin, .. } if pin == "BlueRidge")),
            "the refolded wake rings at boot: {effects:?}"
        );

        // The brain journals the ring; a host tick alone cannot.
        crate::doorbell::journal_append_workspace(
            &crate::config::state_dir(),
            workspace,
            &workspace::Msg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
            &[
                workspace::Cmd::Audit {
                    kind: "drained".into(),
                    pin: "BlueRidge".into(),
                    detail: "1 deferred wake(s) at prompt".into(),
                },
                workspace::Cmd::Prompt {
                    pin: "BlueRidge".into(),
                    line: crate::doorbell::deferred_wake_line(),
                    event_id: None,
                },
            ],
            serde_json::json!({ "stage": "drain" }),
            Some(&["BlueRidge".to_string()]),
        );
        let (tx2, _rx2) = mpsc::channel(8);
        let mut after = RtHost::new(Config::default(), &project, workspace, tx2).expect("host");
        after.tick(&BrainMsg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        });
        after.tick(&BrainMsg::Sweep { now: 1 });
        let effects = after.tick(&BrainMsg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Prompt { .. })),
            "a drained wake is never re-rung: {effects:?}"
        );
    }

    /// The hear tracker gets the pin's npub from the self index, so the
    /// agent's own reply (a watch event authored by that npub) clears the
    /// unanswered ring. With the trait default (`None`) the ring is
    /// npub-less and SwiftBay stayed unanswered after answering.
    #[test]
    fn pin_npub_resolves_through_the_self_index() {
        let _home = crate::config::test_state_home();
        let _mail = crate::mail::Mail::from_config(None)
            .expect("in-process mail")
            .expect("some");
        let (tx, _rx) = mpsc::channel(8);
        let mut host =
            RtHost::new(Config::default(), "offrt-npub", "offrt-npub", tx).expect("host");
        host.model
            .doorbell
            .index
            .insert_pin("SwiftBay".into(), Some("AB12".into()));
        assert_eq!(host.pin_npub("SwiftBay").as_deref(), Some("ab12"));
        assert_eq!(host.pin_npub("NoSuchPin"), None);
    }

    /// bd-0uzrq.18: the watcher index is the union of the pins pane status
    /// already taught the model and the npub-rich registry build. Replacing it
    /// would forget an attached pin; skipping the merge would leave the hex
    /// `p` tag of every mail-delivered wake unresolved.
    #[test]
    fn merge_index_keeps_status_pins_and_adds_registry_npubs() {
        let mut into = doorbell::SelfIndex::from_pins(["AttachedPin"]);
        let mut from = doorbell::SelfIndex::default();
        from.insert_pin("RegistryPin".into(), Some("AB12".into()));
        from.insert_pin("AttachedPin".into(), Some("cd34".into()));
        merge_index(&mut into, &from);
        assert_eq!(into.resolve("ab12").as_deref(), Some("RegistryPin"));
        assert_eq!(into.resolve("cd34").as_deref(), Some("AttachedPin"));
        assert!(into.resolve("AttachedPin").is_some());
    }

    /// bd-0uzrq.18: the daemon's wake socket is the live door when it exists.
    /// The in-process bus is only the no-daemon fallback, so a mail published
    /// in the daemon process is not shadowed by a subscription that never
    /// sees it.
    #[test]
    fn in_process_bus_is_only_armed_without_a_wake_socket() {
        let dir = std::env::temp_dir().join(format!("offrt-wake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("wake.sock");
        assert!(arm_in_process_bus(&socket));
        std::fs::write(&socket, b"").unwrap();
        assert!(!arm_in_process_bus(&socket));
        let _ = std::fs::remove_file(&socket);
    }

    /// bd-0uzrq.18: the constructor must put the npub-rich index where the
    /// reducer classifies, not in a side field. With the model index built
    /// from pin names alone, the hex `p` tag of a mail-delivered wake resolved
    /// zero targets and no Prompt was emitted; this asserts the ring that only
    /// the merged index can produce.
    #[test]
    fn host_construction_makes_a_hex_npub_mail_ring() {
        let _home = crate::config::test_state_home();
        let mail = crate::mail::Mail::from_config(None)
            .expect("in-process mail")
            .expect("some");
        let project = "offrt-npub-ring";
        let key = crate::state::spawn_registry_key(project);
        let agents = config::state_dir().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join(format!("{key}.json")),
            serde_json::json!([{"pane_id": "p1", "name": "h1", "mail_identity": "SwiftBay"}])
                .to_string(),
        )
        .unwrap();
        mail.register_agent(project, "SwiftBay", "test-harness", "m", "t")
            .expect("register");
        let npub = mail.whois(project, "SwiftBay").expect("whois")["npub"]
            .as_str()
            .expect("npub")
            .to_string();

        let (tx, _rx) = mpsc::channel(8);
        let mut host =
            RtHost::new(Config::default(), project, "offrt-npub-ring", tx).expect("host");
        host.tick(&BrainMsg::PlaneStatus {
            pin: "SwiftBay".into(),
            status: "idle".into(),
        });
        let line = format!(
            r#"{{"kind":9,"id":"wake-npub-1","tags":[["p","{npub}"],["t","{project}"],["toron-kind","mail-delivered"],["mid","msg-npub-1"]],"content":"[doorbell] mail from SwiftBay thread t importance normal"}}"#
        );
        let effects = host.tick(&BrainMsg::Watch(line));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Prompt { pin, .. } if pin == "SwiftBay")),
            "the merged index must ring the hex npub: {effects:?}"
        );
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
        let _ = std::fs::remove_file(doorbell::state_file(&config::state_dir(), ws));
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

#[cfg(test)]
mod boot_drain_tests {
    //! bd-evuag: a pin that went idle during a host restart keeps its journaled
    //! deferrals forever if the new host only drains on a working->idle edge.
    use super::*;

    /// An unpulled deferral for a pin already idle at boot must ring on the
    /// boot refold sweep — no working->idle edge will ever arrive.
    #[test]
    fn boot_refold_drains_a_deferral_for_a_pin_already_idle() {
        let _home = crate::config::test_state_home();
        let project = format!("boot-drain-{}", std::process::id());
        let workspace = format!("session:boot-drain-{}", std::process::id());
        let state_dir = crate::config::state_dir();
        let pin = "AmberProbe";
        let mut st = crate::doorbell::WakeState::default();
        st.since_by_pin
            .insert(pin.to_string(), "2023-11-14T22:14:00.000000Z".into());
        crate::doorbell::save_wake_state(
            &crate::doorbell::wake_state_file(&state_dir, &workspace),
            &st,
        )
        .expect("save cursor older than the deferral");
        crate::doorbell::journal_deferred_wake_with_wm(
            &state_dir,
            &workspace,
            pin,
            &crate::doorbell::deferred_wake_line(),
            &[],
            Some(u64::MAX), // unpulled: wm far ahead of the cursor
        )
        .expect("journal the handoff deferral");
        state::save_spawn_registry(
            &project,
            &[crate::state::SpawnEntry::with_mail_identity(
                "block:boot-1".to_string(),
                "pane-boot-1".to_string(),
                pin.to_string(),
            )],
        )
        .expect("map pin to a pane");

        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), &project, &workspace, tx).expect("host");
        // The pin is ALREADY idle at boot (it went idle during the restart gap).
        host.tick(&BrainMsg::PlaneStatus {
            pin: pin.to_string(),
            status: "idle".into(),
        });
        let effects = host.tick(&BrainMsg::Sweep { now: 1 });
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Prompt { pin: p, .. } if p == pin)),
            "an idle-at-boot pin with an unpulled deferral must be drained at boot: {effects:?}"
        );
    }

    /// The converse: a deferral already pulled before boot must NOT ring on
    /// the boot sweep (bd-fwt8r cursor check), even for an idle-at-boot pin.
    #[test]
    fn boot_refold_skips_a_deferral_already_pulled_before_boot() {
        let _home = crate::config::test_state_home();
        let project = format!("boot-pulled-{}", std::process::id());
        let workspace = format!("session:boot-pulled-{}", std::process::id());
        let state_dir = crate::config::state_dir();
        let pin = "AmberProbe";
        let mut st = crate::doorbell::WakeState::default();
        st.since_by_pin
            .insert(pin.to_string(), "2999-01-01T00:00:00.000000Z".into()); // cursor ahead
        crate::doorbell::save_wake_state(
            &crate::doorbell::wake_state_file(&state_dir, &workspace),
            &st,
        )
        .expect("save pulled cursor");
        crate::doorbell::journal_deferred_wake_with_wm(
            &state_dir,
            &workspace,
            pin,
            &crate::doorbell::deferred_wake_line(),
            &[],
            Some(1),
        )
        .expect("journal the handoff deferral");
        state::save_spawn_registry(
            &project,
            &[crate::state::SpawnEntry::with_mail_identity(
                "block:boot-2".to_string(),
                "pane-boot-2".to_string(),
                pin.to_string(),
            )],
        )
        .expect("map pin to a pane");

        let (tx, _rx) = mpsc::channel(8);
        let mut host = RtHost::new(Config::default(), &project, &workspace, tx).expect("host");
        host.tick(&BrainMsg::PlaneStatus {
            pin: pin.to_string(),
            status: "idle".into(),
        });
        let effects = host.tick(&BrainMsg::Sweep { now: 1 });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Prompt { pin: p, .. } if p == pin)),
            "a pulled deferral must not ring at boot: {effects:?}"
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
        backend.prompt(&pane, line, None).map(|_| ()).map_err(|e| {
            // bd-9mjla: a mid-turn pane is a DEFERRAL, not a delivery
            // failure. The reducer keeps the wake and rings it at the next
            // at-prompt status; anything else stays a loud prompt failure.
            let busy = matches!(
                e.code(),
                toron_plane::backend::BackendErrorCode::AgentNotIdle
            ) || matches!(
                e.code(),
                toron_plane::backend::BackendErrorCode::Other(code) if code == "busy"
            ) || e.message.contains("not at a prompt");
            if busy {
                doorbell_rt::RtError::PaneBusy(pin.to_string())
            } else {
                doorbell_rt::RtError::PromptFailed(format!("{e}"))
            }
        })
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
            crate::workspace::Cmd::Prompt { pin, line, .. } => {
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
            crate::workspace::Cmd::FetchInbox
            | crate::workspace::Cmd::CheckAckSla
            | crate::workspace::Cmd::ReadToronCursor { .. } => {
                let Some(services) = services else {
                    return Self::interpret(cmd);
                };
                let kind: &'static str = match cmd {
                    crate::workspace::Cmd::FetchInbox => "fetch_inbox",
                    crate::workspace::Cmd::ReadToronCursor { .. } => "read_toron_cursor",
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
            event_id: None,
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
            event_id: None,
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
            event_id: None,
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

#[cfg(test)]
mod bd_346jl_binding_tests {
    use super::*;

    /// A cross-session rex block bound by `flywheel arrive` has no spawn row;
    /// the identity bridge is the only thing that names its pin, and without
    /// it the pane is "unmapped" and its at-prompt transition never fires.
    #[test]
    fn a_bound_block_without_a_spawn_row_still_maps_to_its_pin() {
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("block:crew".into(), "CrewPin".into());
        let panes = vec![
            "block:crew".to_string(),
            "block:01a1c323-captain".to_string(),
            "block:unknown".to_string(),
        ];
        let extra = RtHost::bindings_for_unmapped(&map, &panes, |pane| {
            (pane == "block:01a1c323-captain").then(|| "SlateHarbor".to_string())
        });
        assert_eq!(
            extra,
            vec![(
                "block:01a1c323-captain".to_string(),
                "SlateHarbor".to_string()
            )],
            "the bridge maps the foreign block; the unbound one stays unmapped"
        );
        for (pane, pin) in extra {
            map.insert(pane, pin);
        }
        let agents = vec![
            site_agent_for("block:01a1c323-captain", "idle"),
            site_agent_for("block:unknown", "idle"),
        ];
        let (updates, unmapped) = RtHost::presence_updates_for(&agents, &map);
        assert_eq!(
            updates.len(),
            1,
            "the captain block is observed: {updates:?}"
        );
        assert_eq!(unmapped, vec!["block:unknown".to_string()]);
    }

    fn site_agent_for(pane: &str, status: &str) -> toron_core::pane_types::AgentInfo {
        let mut row = toron_core::pane_types::AgentInfo {
            agent: None,
            agent_status: toron_core::pane_types::AgentStatus::Idle,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: None,
            pane_id: pane.to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: String::new(),
            terminal_id: String::new(),
            terminal_title: None,
            workspace_id: String::new(),
        };
        row.agent_status = status.parse().expect("status");
        row
    }
}

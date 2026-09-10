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
use crate::herdr_shim::{Client, Subscription};
use crate::{config, doorbell, state, workspace};
use doorbell_rt::{BrainMsg, Effect, FileStore, Host};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
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
    fn send(&mut self, pin: &str, line: &str) -> anyhow::Result<()> {
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
            anyhow::bail!("prompt for {pin} delivered nowhere");
        }
        Ok(())
    }

    fn name(&self) -> &str {
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
    covered_panes: HashSet<String>,
    retry: doorbell_rt::WatchRetry,
    // F7: cache the whois-backed index — rebuild at most every 60s or when
    // the registry pin set changes (was every 1s sweep: N whois RPCs/sec).
    index_refreshed_at: u64,
    index_pins: Vec<String>,
}

impl RtHost {
    pub fn new(
        cfg: Config,
        project: &str,
        workspace: &str,
        brain_tx: mpsc::Sender<BrainMsg>,
    ) -> anyhow::Result<Self> {
        let mut model = workspace::WorkspaceModel::default();
        if let Some(pin) = state::load_captain_alias(project) {
            model.roles.insert("captain".into(), pin);
        }
        // Same seeds as the old loop head (fail-loud on I/O, never skip).
        crate::durable::seed_workspace_from_active_loop(&mut model, project)?;
        crate::seed_roles_from_binds(&mut model, project)?;
        crate::rebuild_assignment_from_receipts(&mut model);
        model.last_now = unix_now();
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
            covered_panes: HashSet::new(),
            retry: doorbell_rt::WatchRetry::default(),
            index_refreshed_at: now,
            index_pins,
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
    fn cover_new_panes(&mut self) {
        let registry = state::load_spawn_registry(&self.project);
        let pane_ids: Vec<(String, String)> = registry
            .iter()
            .filter(|e| !e.pane_id.is_empty())
            .filter(|e| !self.covered_panes.contains(&e.pane_id))
            .filter_map(|e| e.mail_identity.clone().map(|pin| (e.pane_id.clone(), pin)))
            .collect();
        if pane_ids.is_empty() {
            return;
        }
        let tx = self.brain_tx.clone();
        let socket = self.cfg.socket_path();
        let mut ok_panes = Vec::new();
        // One long-lived events.subscribe socket per call would be ideal;
        // per-pane threads match the old topology exactly (proven live).
        for (pane_id, _pin) in &pane_ids {
            let tx_p = tx.clone();
            let map = snapshot_pin(&registry, pane_id);
            let Ok(mut sub_client) = Client::connect(Some(&socket)) else {
                continue;
            };
            let subs = vec![Subscription::agent_status_changed(pane_id.clone())];
            let Ok(mut stream) = crate::herdr_shim::subscribe(&mut sub_client, &subs) else {
                continue;
            };
            ok_panes.push(pane_id.clone());
            std::thread::spawn(move || {
                while let Ok(Some(ev)) = stream.read_event() {
                    if let crate::herdr_shim::HerdrEventData::PaneAgentStatusChanged(change)
                    | crate::herdr_shim::HerdrEventData::PaneAgentStatusChangedSub(change) =
                        ev.data
                    {
                        // Resolve the pin at send time from the spawn-time
                        // snapshot (registry reloads ride the next sweep).
                        let pin = map.clone().unwrap_or_else(|| change.pane_id.clone());
                        let msg = BrainMsg::HerdrStatus {
                            pin,
                            status: change.agent_status.as_str().to_string(),
                        };
                        if tx_p.blocking_send(msg).is_err() {
                            break;
                        }
                    }
                }
            });
        }
        self.covered_panes.extend(ok_panes);
    }
}

fn snapshot_pin(registry: &[state::SpawnEntry], pane_id: &str) -> Option<String> {
    registry
        .iter()
        .find(|e| e.pane_id == pane_id)
        .and_then(|e| e.mail_identity.clone())
}

fn build_index(cfg: &Config, project: &str) -> doorbell::SelfIndex {
    let pins: Vec<String> = state::load_spawn_registry(project)
        .iter()
        .filter_map(|e| e.mail_identity.clone())
        .collect();
    crate::doorbell_self_index(cfg, project, &pins)
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
            BrainMsg::Sweep { since, now } => (
                workspace::Msg::Sweep {
                    since: since.clone(),
                    now: *now,
                },
                true,
            ),
            BrainMsg::RoleBound { role, pin } => (
                workspace::Msg::RoleBound {
                    role: role.clone(),
                    pin: pin.clone(),
                },
                false,
            ),
            BrainMsg::Fact { summary } => (
                workspace::Msg::Fact {
                    summary: summary.clone(),
                },
                false,
            ),
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
            // Rings for overdue posts arrive via the watch path (the scan
            // emits real channel posts, exactly like the old loop) — the
            // SlaFired tick itself decides nothing.
            BrainMsg::SlaFired { .. }
            | BrainMsg::Degraded { .. }
            | BrainMsg::Delivered { .. }
            | BrainMsg::Ask { .. }
            | BrainMsg::Shutdown => return Vec::new(),
        };
        // The tick is the decider; the effect layer runs, then intents map.
        let (next, cmds) = workspace::tick(&self.model, &wmsg);
        self.model = next;
        crate::handle_workspace_cmds(
            &mut self.model,
            &cmds,
            &self.project,
            &self.cfg,
            drain_waits,
        );
        cmds.into_iter()
            .filter_map(|c| match c {
                workspace::Cmd::Prompt { pin, line } => Some(Effect::Prompt { pin, line }),
                // Pin is advisory: the brain fetches every known pin with
                // per-pin cursors (cursor-safe), matching the old sweep arm.
                workspace::Cmd::FetchInbox { .. } => Some(Effect::FetchInbox {
                    pin: String::new(),
                    since: String::new(),
                }),
                workspace::Cmd::CheckAckSla => Some(Effect::CheckAckSla),
                workspace::Cmd::RestartWatch => Some(Effect::RestartWatch),
                workspace::Cmd::FactMiss { why } => Some(Effect::Audit {
                    kind: "fact_miss".into(),
                    detail: serde_json::json!({ "why": why }),
                }),
                workspace::Cmd::Audit { kind, pin, detail } => Some(Effect::Audit {
                    kind,
                    detail: serde_json::json!({ "pin": pin, "detail": detail }),
                }),
                _ => None,
            })
            .collect()
    }

    fn now_unix(&self) -> u64 {
        unix_now()
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
        match crate::mail_shim::block_on(mb.check_ack_sla_expired(&self.project, None)) {
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
        // ticks Presence per row; hear derives after).
        let path = crate::mail_shim::mailbox_roots().0.join("presence.jsonl");
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
        crate::drain_pending_facts(&self.project)
    }

    fn restart_watch(&mut self) {
        // GS-007 never-deaf rule: burst in a task, gate across calls.
        let backoff = self.retry.record_fail();
        let tx = self.brain_tx.clone();
        let project = self.project.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            let tx_g = tx.clone();
            match crate::spawn_watch_child(&project, move |ev| {
                let _ = match ev {
                    Some(l) => tx_g.blocking_send(BrainMsg::Watch(l)),
                    None => tx_g.blocking_send(BrainMsg::Died),
                };
            }) {
                Some(_child) => {
                    doorbell::audit_append(
                        &config::state_dir(),
                        "",
                        serde_json::json!({ "kind": "watch_restart" }),
                    );
                }
                None => {
                    let _ = tx.blocking_send(BrainMsg::Died);
                }
            }
        });
    }
}

impl RtHost {
    fn fetch_inbox_process(&self, pin: &str, since: &str) -> Option<Vec<serde_json::Value>> {
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
    sinks.add_sink(
        "herdr",
        Box::new(HerdrPromptSink::new(
            &cfg,
            project,
            workspace,
            generation.clone(),
        )),
    );
    let store = FileStore::new(config::state_dir());
    // Watch source: in-process bus watch first (zero fork), CLI child fallback.
    let mut watch_child: Option<std::process::Child> = None;
    if !spawn_bus_watch(&cfg, project, tx.clone()) {
        let tx_w = tx.clone();
        let project = project.to_string();
        watch_child = crate::spawn_watch_child(&project, move |ev| {
            let _ = match ev {
                Some(l) => tx_w.blocking_send(BrainMsg::Watch(l)),
                None => tx_w.blocking_send(BrainMsg::Died),
            };
        });
    }
    host.cover_new_panes();
    // Beats: 1s sweep, 60s ack-SLA (same cadences as the old loop).
    let sweep_tx = tx.clone();
    let sweep_ticker =
        doorbell_rt::interval_ticker("sweep", sweep_tx, Duration::from_secs(1), || {
            BrainMsg::Sweep {
                since: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                now: unix_now(),
            }
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
    // Beads-ledger producer (bd-asr2.15, D7, flag-gated): read-only WAL
    // poll emitting Fact Msgs. Repo root is the spawn-time cwd.
    let _beads_poller = cfg.doorbell_beads_poller().then(|| {
        let repo = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
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
    if let Some(mut c) = watch_child {
        let _ = c.kill();
    }
    eprintln!(
        "flywheel doorbell: embedded loop exited (injects={} sheds={} degraded={})",
        report.stats.injects, report.stats.sheds, report.stats.degraded
    );
    Ok(())
}

/// In-process bus watch (zero fork): kinds mirror the CLI fallback filter.
fn spawn_bus_watch(cfg: &Config, project: &str, tx: mpsc::Sender<BrainMsg>) -> bool {
    (|| -> Option<()> {
        let _mail = crate::mail_shim::Mail::from_config(Some(&cfg.toron_mail_config()))
            .ok()
            .flatten()?;
        let bus = crate::mail_shim::block_on(async {
            Ok::<std::sync::Arc<dyn toron_core::bus::Bus>, anyhow::Error>(
                crate::mail_shim::mailbox().bus().clone(),
            )
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
        std::thread::spawn(move || {
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
                        if let Ok(line) = serde_json::to_string(&v)
                            && tx.blocking_send(BrainMsg::Watch(line)).is_err()
                        {
                            break;
                        }
                    }
                    Some(toron_core::bus::SubscriptionEvent::Eose) => continue,
                    Some(toron_core::bus::SubscriptionEvent::Closed(_)) | None => {
                        let _ = tx.blocking_send(BrainMsg::Died);
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
    use doorbell_rt::{Brain, BrainConfig, BrainMsg, FileHost, FileHostConfig, MockSink, SinkManager};

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
        let raw = std::fs::read_to_string(dir.join("journals").join(format!(
            "doorbell-journal-{ws}.jsonl"
        )))
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
                doorbell_rt::file_host::registry_key(proj),
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
        let toron_dir =
            std::env::temp_dir().join(format!("parity-toron-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&toron_dir);
        std::fs::create_dir_all(&toron_dir).unwrap();
        // SAFETY: under ENV_LOCK via _home; restored below.
        unsafe {
            std::env::set_var("TORON_DATA_DIR", &toron_dir);
            std::env::remove_var("TORON_MCP_DATA_DIR");
        }
        let proj_dir =
            std::env::temp_dir().join(format!("parity-proj-{}", std::process::id()));
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
                serde_json::json!({"name": "PinA", "state": "idle", "ts": now}).to_string(),
                serde_json::json!({"name": "PinB", "state": "idle", "ts": now}).to_string(),
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

        // One scripted Msg log; fixed `since`/`now` payloads so only `ts`
        // differs between runs. Built twice (BrainMsg is not Clone).
        let inbox_row = |ts: &str| {
            serde_json::json!({"importance": "urgent", "created_ts": ts, "tags": []})
        };
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
                BrainMsg::Watch(watch(
                    9,
                    serde_json::json!([["p", "PinA"]]),
                    "e1",
                    1,
                )),
                BrainMsg::Watch(watch(
                    9,
                    serde_json::json!([["p", "PinB"]]),
                    "e2",
                    2,
                )),
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
                BrainMsg::Sweep {
                    since: "2026-09-10T08:00:00Z".into(),
                    now: 1_700_000_000,
                },
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
                BrainMsg::Watch(watch(
                    9,
                    serde_json::json!([["p", "PinA"]]),
                    "e4",
                    4,
                )),
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
            let mut cfg = crate::config::Config::default();
            cfg.mail = Some(crate::config::MailSections {
                agentmail: None,
                toron: Some(crate::config::MailServerConfig {
                    url: Some("http://127.0.0.1:9/mcp".into()),
                    token: None,
                }),
            });
            cfg
        }

        // Side A: embedded host.
        let state_a =
            std::env::temp_dir().join(format!("parity-state-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_a);
        set_xdg(&state_a);
        write_fixture(&state_a, &key, &role_key);
        let dir_a =
            std::env::temp_dir().join(format!("parity-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_a);
        let (tx_a, rx_a) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        let rt_host =
            RtHost::new(remote_cfg(), &project, ws, tx_a.clone())
                .expect("rthost builds in sandbox");
        // Tick-level AckSla parity on throwaways (scan excluded live).
        {
            use doorbell_rt::Host;
            let mut rt = RtHost::new(remote_cfg(), &project, ws, tx_a.clone())
                .expect("throwaway rthost");
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
        let state_b =
            std::env::temp_dir().join(format!("parity-state-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_b);
        set_xdg(&state_b);
        write_fixture(&state_b, &key, &role_key);
        let dir_b =
            std::env::temp_dir().join(format!("parity-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir_b);
        let (tx_b, rx_b) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        let mut fcfg = FileHostConfig::new(
            crate::config::state_dir(),
            project.clone(),
            ws.to_string(),
        );
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
        assert!(fx
            .iter()
            .any(|e| matches!(e, doorbell_rt::Effect::RestartWatch)));
    }
}

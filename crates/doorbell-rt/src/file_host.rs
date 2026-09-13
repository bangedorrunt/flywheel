// governed-by: ADR-0012
//! FileHost — daemon-native [`Host`](crate::host::Host) (bd-ene2 Phase 3).
//!
//! Same [`Brain`], file/CLI inputs: flywheel state files (spawn registry,
//! role binds, pending role-bounds/facts, pid/gen) plus the `toron` /
//! `herdr` CLIs (watch, inbox, whois, prompt, agent list). Zero new crates:
//! `std::process` only. No flywheel types — this is what lets the
//! `doorbell` daemon host the loop without linking the orchestrator.
//!
//! Parity contract (bd-ene2 verify): the same [`BrainMsg`] sequence with
//! the same registry/presence/inbox inputs yields the same [`Effect`]
//! sequence as the embedded `RtHost`. Rules: classification is
//! `doorbell-core` verbatim (via [`CoreReducer`]); the presence prompt
//! filter mirrors `workspace::filter_prompts` (Idle keeps, Busy+urgent
//! breaks through, else `prompt_filtered` Audit — same kind/pin/detail
//! text); `FetchInbox` effects normalize to empty pin/since like the
//! embedded advisory mapping. The former G1 gap (`scan_ack_sla` returned
//! empty for lack of a toron CLI verb) is closed: the daemon shells
//! `toron ack-sla --project` and relays the expired ids (bd-0zcr.2).

use crate::host::{CoreReducer, Effect, Host};
use crate::msg::BrainMsg;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Toron watch kinds — MUST match the embedded in-process filter and the
/// CLI fallback (`DOORBELL_WATCH_KINDS` in flywheel `src/main.rs`).
pub const WATCH_KINDS: &str = "9,14,20001,30013,43001,30031,39002";

/// Presence projection (<30s freshness) — mirrors `timeout_presence_rows`.
pub const PRESENCE_FRESH_SECS: u64 = 30;
/// Index rebuild cadence — mirrors the embedded 60s TTL + pin-set change.
pub const INDEX_TTL_SECS: u64 = 60;

/// Presence verdict subset the tick path stores (mirrors
/// `workspace::Presence::from_str`: wire words only, never Dead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pres {
    Idle,
    Busy,
    Unknown,
}

fn pres_from_word(s: &str) -> Pres {
    match s.trim().to_ascii_lowercase().as_str() {
        "working" | "busy" | "blocked" => Pres::Busy,
        "idle" | "done" => Pres::Idle,
        _ => Pres::Unknown,
    }
}

/// Minimal generation record — same JSON shape as flywheel's
/// `WatcherGen` (`started_unix`, `binary_mtime_unix`, `binary_len`,
/// `binary_hash` sha1-of-first-8k, empty = pre-hash).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GenRecord {
    started_unix: u64,
    binary_mtime_unix: u64,
    binary_len: u64,
    #[serde(default)]
    binary_hash: String,
}

/// Bounded CLI run (bd-ene2): a hung child must wedge neither the brain
/// sweep nor shutdown. Polls `try_wait`; kills past the deadline; reaps.
/// Pipe backpressure bound: outputs stay small (50-row inbox caps);
/// anything bigger is killed, never awaited.
pub fn run_cli(bin: &str, args: &[&str], timeout_secs: u64) -> Option<std::process::Output> {
    let mut child = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Per-call ceiling for brain-task CLIs (inbox, list, whois).
pub const CLI_TIMEOUT_SECS: u64 = 15;
fn file_identity(exe: &Path) -> Option<(u64, u64)> {
    let md = std::fs::metadata(exe).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, md.len()))
}

fn file_hash(exe: &Path) -> Option<String> {
    use sha1::Digest;
    let mut f = std::fs::File::open(exe).ok()?;
    let mut buf = [0u8; 8192];
    use std::io::Read;
    let n = f.read(&mut buf).ok()?;
    let mut h = sha1::Sha1::new();
    h.update(&buf[..n]);
    Some(hex::encode(h.finalize()))
}

fn gen_stale(record: &GenRecord, exe: &Path) -> bool {
    match file_identity(exe) {
        Some((mtime, len)) => {
            if !record.binary_hash.is_empty() {
                match file_hash(exe) {
                    Some(h) => h != record.binary_hash,
                    None => true,
                }
            } else {
                record.binary_mtime_unix != mtime || record.binary_len != len
            }
        }
        None => true,
    }
}

/// Capture the daemon generation record on first hosting: same rule as
/// the embedded WatcherGen gate (mtime+len+sha1-of-8k), daemon-keyed
/// (`{workspace}.daemon.gen` beside the pid/gen files).
pub fn ensure_daemon_gen(flywheel_state: &Path, workspace: &str) {
    let path = flywheel_state
        .join("doorbell")
        .join(format!("{workspace}.daemon.gen"));
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("doorbell"));
    // Refresh when the RUNNING binary no longer matches the record, not
    // only when the file is missing. The gen gate exists to retire a
    // watcher whose binary changed UNDER it mid-run; a daemon started
    // AFTER a binary upgrade must re-arm against its own (new) exe —
    // keeping the stale record made `inject_allowed` permanently false,
    // so every budget-granted prompt was silently eaten (grants journaled,
    // zero panes touched).
    if path.is_file()
        && let Ok(raw) = std::fs::read(&path)
        && let Ok(record) = serde_json::from_slice::<GenRecord>(&raw)
        && !gen_stale(&record, &exe)
    {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = GenRecord {
        started_unix: now,
        binary_mtime_unix: file_identity(&exe).map(|(m, _)| m).unwrap_or(0),
        binary_len: file_identity(&exe).map(|(_, l)| l).unwrap_or(0),
        binary_hash: file_hash(&exe).unwrap_or_default(),
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_vec(&record) {
        let _ = std::fs::write(&path, raw);
    }
}

/// Daemon-side addressing: everything the host reads lives under the
/// flywheel state dir except the toron data dir (presence) and the CLIs.
#[derive(Debug, Clone)]
pub struct FileHostConfig {
    /// Flywheel state dir (`.../flywheel`: agents, roles, facts,
    /// doorbell pids/gens, journals).
    pub flywheel_state: PathBuf,
    pub project: String,
    pub workspace: String,
    /// Toron data dir override (presence.jsonl). `None` mirrors the
    /// flywheel resolution: `TORON_DATA_DIR` > `TORON_MCP_DATA_DIR` >
    /// `XDG_DATA_HOME/toron` > `~/.local/share/toron`.
    pub toron_data_dir: Option<PathBuf>,
    pub toron_bin: String,
    pub herdr_bin: String,
    /// Registry file override (tests/sandbox). `None` resolves
    /// `agents/{key}.json` under the flywheel state dir.
    pub registry_file: Option<PathBuf>,
}

impl FileHostConfig {
    pub fn new(
        flywheel_state: impl Into<PathBuf>,
        project: impl Into<String>,
        workspace: impl Into<String>,
    ) -> Self {
        Self {
            flywheel_state: flywheel_state.into(),
            project: project.into(),
            workspace: workspace.into(),
            toron_data_dir: None,
            toron_bin: "toron".into(),
            herdr_bin: "herdr".into(),
            registry_file: None,
        }
    }
}

/// Resolve the toron mailbox data ROOT the way flywheel's `mailbox_roots`
/// does: explicit override > `TORON_DATA_DIR` > `TORON_MCP_DATA_DIR` >
/// `$XDG_DATA_HOME/toron` > `~/.local/share/toron`. Free fn so `/health`
/// can report the root the daemon actually watches — a daemon on a
/// different mailbox than the pins is deaf, and this is the field that
/// makes that visible.
pub fn toron_data_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(d) = explicit {
        return d.to_path_buf();
    }
    for var in ["TORON_DATA_DIR", "TORON_MCP_DATA_DIR"] {
        if let Ok(d) = std::env::var(var)
            && !d.trim().is_empty()
        {
            return PathBuf::from(d.trim());
        }
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("toron")
}

/// Canonical presence projection under a toron data ROOT: `<root>/presence.jsonl`.
/// Toron lifts the mailbox data dir (`<root>/projects`) back to the root via
/// `mailbox_root` (bd-m5ym), so rows land HERE — never at
/// `<root>/projects/presence.jsonl`, which nothing writes.
pub fn presence_path(toron_data_root: &Path) -> PathBuf {
    toron_data_root.join("presence.jsonl")
}

/// fnv1a64 — mirrors flywheel `state::fnv1a64` for registry keys.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Registry key — mirrors `state::state_key`: slash paths hash to
/// `{base}-{8hex}`, bare slugs pass through.
pub fn registry_key(project: &str) -> String {
    if project.contains('/') {
        let base = project.rsplit('/').next().unwrap_or(project);
        let h = fnv1a64(project);
        format!("{base}-{:08x}", (h & 0xffff_ffff) as u32)
    } else {
        project.to_string()
    }
}

/// Resolve the registry file: explicit override, `{slug}.json`, or the
/// single `{slug}-*.json` hashed candidate (sorted-first + ambiguous
/// flag when several match). Slug matching uses `registry_key`, which
/// mirrors flywheel `state::state_key`: absolute-path projects hash to
/// `{base}-{8hex}` (e.g. `proj-0ec6cde6`). Matching the RAW project string
/// instead meant an absolute-path project could never resolve its own
/// registry — the daemon saw zero pins, resolved no p-tag npubs, and every
/// wake classified to zero targets (the silent-deaf root cause).
pub fn resolve_registry_file(
    flywheel_state: &Path,
    project: &str,
    override_path: Option<&Path>,
) -> (Option<PathBuf>, bool) {
    if let Some(p) = override_path {
        return (Some(p.to_path_buf()), false);
    }
    let slug = registry_key(project);
    let agents = flywheel_state.join("agents");
    let plain = agents.join(format!("{slug}.json"));
    if plain.is_file() {
        return (Some(plain), false);
    }
    let Ok(entries) = std::fs::read_dir(&agents) else {
        return (None, false);
    };
    let mut hits: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("{slug}-")) && n.ends_with(".json"))
        })
        .collect();
    hits.sort();
    // bd-ztek: several hashed files routinely exist for one project slug
    // (absolute vs relative spellings hash differently, each holding a
    // different spawn's pins). First-sorted silently swept a stale fleet's
    // pins. Prefer the most recently modified file — the live spawn writes
    // its registry at spawn — with name order only breaking mtime ties.
    hits.sort_by(|a, b| {
        let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        mtime(b).cmp(&mtime(a)).then_with(|| a.cmp(b))
    });
    let ambiguous = hits.len() > 1;
    (hits.into_iter().next(), ambiguous)
}

/// A `toron watch` child that is killed + reaped when dropped. Wrapping is
/// the point: a bare `std::process::Child` drop leaves the process running,
/// which is how `Died`-driven restarts littered long-lived watchers.
struct WatchChild(std::process::Child);

impl Drop for WatchChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub struct FileHost {
    cfg: FileHostConfig,
    reducer: CoreReducer,
    /// Sticky presence verdicts — ONLY `Presence` msgs write here
    /// (mirrors the workspace tick arm: direct insert, no fallback).
    presence: BTreeMap<String, Pres>,
    /// Last-known herdr statuses by pin (poll-joined via registry).
    herdr: BTreeMap<String, String>,
    /// Role binds (role -> pin), drained from the pending files at sweep
    /// top exactly like the embedded workspace model. The Fact arm reads
    /// `captain`/`wheelhouse` here (bd-ztek: without it every daemon-side
    /// fact, including inject, degraded to fact_miss).
    roles: BTreeMap<String, String>,
    /// pane_id -> pin for the herdr list join.
    panes: BTreeMap<String, String>,
    /// pin -> herdr agent name for prompt delivery. Shared with the
    /// prompt sink: single writer (index refresh on the Brain task),
    /// brief worker-side reads, never held across await.
    targets: std::sync::Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    /// npub cache for the index (whois on pin-set change + 60s TTL).
    npubs: BTreeMap<String, String>,
    index_pins: Vec<String>,
    index_refreshed_at: u64,
    retry: crate::policy::WatchRetry,
    daemon_exe: PathBuf,
    generation: Option<GenRecord>,
    /// Status-change outbox: the poll loop `tell`s `HerdrStatus` back
    /// through this channel (forwarder topology over a poll transport).
    tx: Option<tokio::sync::mpsc::Sender<BrainMsg>>,
    pending_audit: Option<(String, serde_json::Value)>,
    /// The single live `toron watch` child (shared with the respawn task so
    /// a replacement reaps its predecessor). [`WatchChild`] kills on drop.
    watch_child: std::sync::Arc<std::sync::Mutex<Option<WatchChild>>>,
}

impl Drop for FileHost {
    /// Never outlive a live watcher: dropping the host (shutdown, or a
    /// retired generation) reaps the child instead of orphaning it.
    fn drop(&mut self) {
        if let Ok(mut g) = self.watch_child.lock() {
            let _ = g.take();
        }
    }
}

impl FileHost {
    pub fn new(cfg: FileHostConfig) -> Self {
        let daemon_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("doorbell"));
        let generation = std::fs::read(
            cfg.flywheel_state
                .join("doorbell")
                .join(format!("{}.daemon.gen", cfg.workspace)),
        )
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok());
        Self {
            cfg,
            reducer: CoreReducer::default(),
            presence: BTreeMap::new(),
            herdr: BTreeMap::new(),
            roles: BTreeMap::new(),
            panes: BTreeMap::new(),
            targets: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            npubs: BTreeMap::new(),
            index_pins: Vec::new(),
            index_refreshed_at: 0,
            retry: crate::policy::WatchRetry::default(),
            daemon_exe,
            generation,
            tx: None,
            pending_audit: None,
            watch_child: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Shared prompt routing table for the CLI sink (see field docs).
    pub fn target_table(&self) -> std::sync::Arc<std::sync::Mutex<BTreeMap<String, String>>> {
        self.targets.clone()
    }

    /// Status-change outbox (see field docs).
    pub fn with_tx(mut self, tx: tokio::sync::mpsc::Sender<BrainMsg>) -> Self {
        self.tx = Some(tx);
        self
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn toron_data_dir(&self) -> PathBuf {
        toron_data_root(self.cfg.toron_data_dir.as_deref())
    }

    /// (pin, pane_id, herdr name) rows from the registry file. Ambiguity
    /// queues a pending audit (drained as one Audit effect on next tick).
    fn registry_rows(&mut self) -> Vec<(String, String, String)> {
        let (file, ambiguous) = resolve_registry_file(
            &self.cfg.flywheel_state,
            &self.cfg.project,
            self.cfg.registry_file.as_deref(),
        );
        if ambiguous {
            // bd-ztek: name the chosen file — first-sorted used to sweep a
            // stale fleet's pins with no trace of which file won.
            let chosen = file
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.pending_audit = Some((
                "registry_ambiguous".into(),
                serde_json::json!({ "project": self.cfg.project, "registry_file": chosen }),
            ));
        }
        let Some(path) = file else { return Vec::new() };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| {
                let pin = e.get("mail_identity")?.as_str()?.to_string();
                if pin.is_empty() {
                    return None;
                }
                let pane = e.get("pane_id")?.as_str().unwrap_or("").to_string();
                let name = e.get("name")?.as_str().unwrap_or("").to_string();
                Some((pin, pane, name))
            })
            .collect()
    }

    /// Refresh the reducer index when the pin set changes or the TTL
    /// lapses (mirrors the embedded 60s rule; whois failures keep None).
    fn refresh_index(&mut self) {
        let rows = self.registry_rows();
        let pins: Vec<String> = rows.iter().map(|(p, _, _)| p.clone()).collect();
        let now = Self::now_unix();
        if pins == self.index_pins && now.saturating_sub(self.index_refreshed_at) < INDEX_TTL_SECS {
            return;
        }
        self.panes = rows
            .iter()
            .map(|(pin, pane, _)| (pane.clone(), pin.clone()))
            .collect();
        if let Ok(mut targets) = self.targets.lock() {
            targets.clear();
            targets.extend(
                rows.iter()
                    .map(|(pin, _, name)| (pin.clone(), name.clone())),
            );
        }
        for pin in &pins {
            if self.npubs.contains_key(pin) {
                continue;
            }
            if let Some(npub) = self.whois(pin)
                && !npub.is_empty()
            {
                self.npubs.insert(pin.clone(), npub);
            }
        }
        self.npubs.retain(|p, _| pins.contains(p));
        self.reducer.model.index = doorbell_core::SelfIndex::default();
        for pin in &pins {
            self.reducer
                .model
                .index
                .insert_pin(pin.clone(), self.npubs.get(pin).cloned());
        }
        self.index_pins = pins;
        self.index_refreshed_at = now;
    }

    fn whois(&self, pin: &str) -> Option<String> {
        let out = run_cli(
            &self.cfg.toron_bin,
            &["mail", "whois", &self.cfg.project, pin, "--json"],
            CLI_TIMEOUT_SECS,
        )?;
        if !out.status.success() {
            return None;
        }
        serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()?
            .get("npub")?
            .as_str()
            .map(str::to_string)
    }

    /// Presence rows: latest state word per pin from presence.jsonl,
    /// <30s fresh (mirrors `timeout_presence_rows`, registry-pinned).
    fn presence_rows(&self, pins: &[String], now: u64) -> Vec<(String, String)> {
        let path = presence_path(&self.toron_data_dir());
        let Ok(body) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut latest: BTreeMap<String, (u64, String)> = BTreeMap::new();
        for line in body.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let (Some(name), Some(ts)) = (
                v.get("name").and_then(|n| n.as_str()),
                v.get("ts").and_then(|t| t.as_u64()),
            ) else {
                continue;
            };
            let state = v
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string();
            latest
                .entry(name.to_string())
                .and_modify(|e| {
                    if ts > e.0 {
                        *e = (ts, state.clone());
                    }
                })
                .or_insert((ts, state));
        }
        latest
            .into_iter()
            .filter(|(pin, (ts, _))| {
                pins.contains(pin) && now.saturating_sub(*ts) < PRESENCE_FRESH_SECS
            })
            .map(|(pin, (_, state))| (pin, state))
            .collect()
    }

    /// herdr statuses joined to pins: `{pane_id: status}` -> `{pin: status}`.
    fn poll_herdr(&self) -> BTreeMap<String, String> {
        let out = run_cli(&self.cfg.herdr_bin, &["agent", "list"], CLI_TIMEOUT_SECS);
        let Some(out) = out else {
            return BTreeMap::new();
        };
        if !out.status.success() {
            return BTreeMap::new();
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let empty = Vec::new();
        let agents = v
            .get("result")
            .and_then(|r| r.get("agents"))
            .and_then(|a| a.as_array())
            .unwrap_or(&empty);
        let mut by_pane: BTreeMap<String, String> = BTreeMap::new();
        for a in agents {
            let (Some(pane), Some(status)) = (
                a.get("pane_id").and_then(|p| p.as_str()),
                a.get("agent_status").and_then(|s| s.as_str()),
            ) else {
                continue;
            };
            by_pane.insert(pane.to_string(), status.to_string());
        }
        by_pane
            .into_iter()
            .filter_map(|(pane, status)| self.panes.get(&pane).map(|pin| (pin.clone(), status)))
            .collect()
    }

    /// Drain pending role bounds: `{state}/roles/{key}-pending.jsonl`
    /// `{role,pin}` lines; file removed after parse (mirrors the
    /// embedded drain: corrupt lines warn, never silently dropped).
    pub fn drain_role_bounds(&mut self) -> Vec<(String, String)> {
        let path = self
            .cfg
            .flywheel_state
            .join("roles")
            .join(format!("{}-pending.jsonl", registry_key(&self.cfg.project)));
        let raw = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(_) => return Vec::new(),
            Ok(raw) => raw,
        };
        let mut out = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => match (
                    v.get("role").and_then(|r| r.as_str()),
                    v.get("pin").and_then(|p| p.as_str()),
                ) {
                    (Some(role), Some(pin)) if !role.is_empty() && !pin.is_empty() => {
                        out.push((role.to_string(), pin.to_string()));
                    }
                    _ => eprintln!("doorbell: warn: corrupt role-bound row skipped"),
                },
                Err(_) => eprintln!("doorbell: warn: corrupt role-bound JSON skipped"),
            }
        }
        let _ = std::fs::remove_file(&path);
        out
    }

    /// Drain pending facts: `{state}/facts/{key}-pending.jsonl` JSON
    /// strings; file removed after read (mirrors the embedded drain).
    pub fn drain_facts(&mut self) -> Vec<String> {
        let path = self
            .cfg
            .flywheel_state
            .join("facts")
            .join(format!("{}-pending.jsonl", registry_key(&self.cfg.project)));
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let _ = std::fs::remove_file(&path);
        raw.lines()
            .filter_map(|l| serde_json::from_str(l.trim()).ok())
            .collect()
    }

    /// Seed role binds: `{state}/role_binds/{key}.json` map (missing →
    /// empty; corrupt → loud error, never silent default).
    pub fn load_binds(&self) -> Result<BTreeMap<String, String>, String> {
        let path = self
            .cfg
            .flywheel_state
            .join("role_binds")
            .join(format!("{}.json", registry_key(&self.cfg.project)));
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(format!("role_binds read failed at {}: {e}", path.display())),
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|e| format!("corrupt role_binds at {}: {e}", path.display())),
        }
    }

    /// Loud file audit — mirrors `doorbell::audit_append` entry shape
    /// (`ts/workspace/kind` + detail) for CLI failures on the `&self`
    /// paths, where no journal row can be emitted inline.
    fn audit_file(&self, kind: &str, detail: serde_json::Value) {
        let path = self
            .cfg
            .flywheel_state
            .join("doorbell")
            .join(format!("{}.audit.jsonl", self.cfg.workspace));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut entry = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "workspace": self.cfg.workspace,
            "kind": kind,
        });
        if let (Some(obj), serde_json::Value::Object(map)) = (entry.as_object_mut(), detail) {
            obj.extend(map);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{entry}");
        }
    }

    /// Presence prompt filter — mirrors `workspace::filter_prompts` over
    /// the sticky presence map: Idle keeps, Busy+urgent breaks through,
    /// everything else becomes a digest-visible `prompt_filtered` Audit
    /// with identical kind/pin/detail text.
    fn apply_presence_filter(&self, effects: Vec<Effect>) -> Vec<Effect> {
        effects
            .into_iter()
            .flat_map(|e| match e {
                Effect::Prompt { pin, line } => match self.presence.get(&pin) {
                    Some(Pres::Idle) => vec![Effect::Prompt { pin, line }],
                    Some(Pres::Busy) if doorbell_core::is_urgent_line(&line) => {
                        vec![Effect::Prompt { pin, line }]
                    }
                    _ => vec![Effect::Audit {
                        kind: "prompt_filtered".into(),
                        detail: serde_json::json!({
                            "pin": pin,
                            "detail": format!(
                                "non-injectable presence held wake for digest: {line}"
                            ),
                        }),
                    }],
                },
                other => vec![other],
            })
            .collect()
    }

    fn normalize(&self, actions: Vec<doorbell_core::DoorbellAction>) -> Vec<Effect> {
        actions
            .into_iter()
            .map(|a| match a {
                doorbell_core::DoorbellAction::Prompt { pin, line } => Effect::Prompt { pin, line },
                // Empty pin/since: the brain fetches every known pin with
                // per-pin cursors (mirrors the embedded advisory mapping).
                doorbell_core::DoorbellAction::FetchInbox { .. } => Effect::FetchInbox {
                    pin: String::new(),
                    since: String::new(),
                },
                doorbell_core::DoorbellAction::CheckAckSla => Effect::CheckAckSla,
                doorbell_core::DoorbellAction::RestartWatch => Effect::RestartWatch,
                doorbell_core::DoorbellAction::FactMiss { why } => Effect::Audit {
                    kind: "fact_miss".into(),
                    detail: serde_json::json!({ "why": why }),
                },
                doorbell_core::DoorbellAction::Audit { kind, pin, detail } => Effect::Audit {
                    kind,
                    detail: serde_json::json!({ "pin": pin, "detail": detail }),
                },
            })
            .collect()
    }
}

impl Host for FileHost {
    fn tick(&mut self, msg: &BrainMsg) -> Vec<Effect> {
        // Live errors surface as one Audit effect on the next tick
        // (never silent; never more than one per error).
        let mut pre = Vec::new();
        if let Some((kind, detail)) = self.pending_audit.take() {
            pre.push(Effect::Audit { kind, detail });
        }
        let core_effects = match msg {
            BrainMsg::Watch(line) => {
                let (next, actions) = doorbell_core::tick(
                    &self.reducer.model,
                    &doorbell_core::DoorbellMsg::WatchNdjson { line: line.clone() },
                );
                self.reducer.model = next;
                self.normalize(actions)
            }
            BrainMsg::HerdrStatus { pin, status } => {
                self.herdr.insert(pin.clone(), status.clone());
                let (next, actions) = doorbell_core::tick(
                    &self.reducer.model,
                    &doorbell_core::DoorbellMsg::HerdrStatus {
                        pin: pin.clone(),
                        status: status.clone(),
                    },
                );
                self.reducer.model = next;
                self.normalize(actions)
            }
            BrainMsg::Died => {
                let (next, actions) = doorbell_core::tick(
                    &self.reducer.model,
                    &doorbell_core::DoorbellMsg::WatchDied,
                );
                self.reducer.model = next;
                self.normalize(actions)
            }
            BrainMsg::Sweep { .. } => vec![Effect::FetchInbox {
                pin: String::new(),
                since: String::new(),
            }],
            BrainMsg::RoleBound { role, pin } => {
                // Mirrors the workspace arm: binds assignment routing only,
                // never the doorbell index — zero classification effect on
                // either host. The bind itself is state the Fact arm reads.
                self.roles.insert(role.clone(), pin.clone());
                Vec::new()
            }
            BrainMsg::Fact { summary } => {
                let (next, actions) = doorbell_core::tick(
                    &self.reducer.model,
                    &doorbell_core::DoorbellMsg::Fact {
                        summary: summary.clone(),
                    },
                );
                self.reducer.model = next;
                // bd-ztek: promote a captain fact to a Prompt exactly like
                // the embedded workspace arm (captain, else wheelhouse, and
                // only over an Idle presence verdict). Without this every
                // daemon-side fact — including `inject` — degraded to
                // fact_miss even with a bound captain.
                if let Some(pin) = self
                    .roles
                    .get("captain")
                    .or_else(|| self.roles.get("wheelhouse"))
                    .cloned()
                    && self.presence.get(&pin) == Some(&Pres::Idle)
                {
                    self.apply_presence_filter(vec![Effect::Prompt {
                        pin,
                        line: format!("[doorbell] {summary}"),
                    }])
                } else {
                    self.normalize(actions)
                }
            }
            BrainMsg::AckSlaTick { now } => {
                let (next, actions) = doorbell_core::tick(
                    &self.reducer.model,
                    &doorbell_core::DoorbellMsg::AckSlaTick { now: *now },
                );
                self.reducer.model = next;
                self.normalize(actions)
            }
            BrainMsg::Presence { pin, state } => {
                // Direct insert (mirrors the workspace tick arm).
                self.presence.insert(pin.clone(), pres_from_word(state));
                Vec::new()
            }
            BrainMsg::InboxFetched { pin, rows } => {
                let (next, actions) = doorbell_core::tick(
                    &self.reducer.model,
                    &doorbell_core::DoorbellMsg::InboxRows {
                        rows: rows.clone(),
                        pin: pin.clone(),
                    },
                );
                self.reducer.model = next;
                self.normalize(actions)
            }
            BrainMsg::SlaFired { .. }
            | BrainMsg::Degraded { .. }
            | BrainMsg::Delivered { .. }
            | BrainMsg::Ask { .. }
            | BrainMsg::Shutdown => Vec::new(),
        };
        pre.into_iter()
            .chain(self.apply_presence_filter(core_effects))
            .collect()
    }

    fn now_unix(&self) -> u64 {
        Self::now_unix()
    }

    fn fetch_inbox(&self, pin: &str, since: &str) -> Vec<serde_json::Value> {
        // `&self` receiver (F6 narrow): failures audit LOUD to the audit
        // file (same entry shape as the embedded path) and yield empty
        // rows — the cursor holds and the batch re-sweeps (at-least-once).
        let out = run_cli(
            &self.cfg.toron_bin,
            &[
                "mail",
                "inbox",
                "--as",
                pin,
                "--project",
                &self.cfg.project,
                "--since",
                since,
                "--limit",
                "50",
            ],
            CLI_TIMEOUT_SECS,
        );
        let Some(out) = out else {
            self.audit_file(
                "inbox_error",
                serde_json::json!({ "pin": pin, "error": "toron spawn/timeout failed" }),
            );
            return Vec::new();
        };
        if !out.status.success() {
            self.audit_file(
                "inbox_error",
                serde_json::json!({
                    "pin": pin,
                    "error": String::from_utf8_lossy(&out.stderr).to_string(),
                }),
            );
            return Vec::new();
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
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

    fn scan_ack_sla(&mut self) -> Vec<String> {
        // bd-0zcr.2 G1 closed: the `toron ack-sla` verb scans + publishes
        // overdue ack wakes; the daemon relays the expired ids it returns
        // and never invents overdue posts. Failures audit LOUD and yield
        // empty — the same at-least-once rule as `fetch_inbox`.
        let out = run_cli(
            &self.cfg.toron_bin,
            &["ack-sla", "--project", &self.cfg.project],
            CLI_TIMEOUT_SECS,
        );
        let Some(out) = out else {
            self.audit_file(
                "ack_sla_error",
                serde_json::json!({ "error": "toron spawn/timeout failed" }),
            );
            return Vec::new();
        };
        if !out.status.success() {
            self.audit_file(
                "ack_sla_error",
                serde_json::json!({
                    "error": String::from_utf8_lossy(&out.stderr).to_string(),
                }),
            );
            return Vec::new();
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
        parsed
            .get("expired")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn restart_watch(&mut self) {
        let backoff = self.retry.record_fail();
        let tx = self.tx.clone();
        let project = self.cfg.project.clone();
        let toron = self.cfg.toron_bin.clone();
        let fw_state = self.cfg.flywheel_state.clone();
        let workspace = self.cfg.workspace.clone();
        // One live watcher per host, never a litter. The slot's `Drop`
        // kills+reaps the previous child before the replacement spawns:
        // dropping a bare `Child` does NOT terminate it, so every Died
        // burst used to leak another long-lived `toron watch` holding a bus
        // subscription (observed live as PPID-1 orphans after restarts).
        let slot = self.watch_child.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            let Some(tx) = tx else { return };
            if let Ok(mut g) = slot.lock() {
                let _ = g.take();
            }
            let kinds = WATCH_KINDS;
            // --idle-exit bounds silence: a half-open bus subscription
            // delivers nothing AND never errors (observed live as a zombie
            // child whose heartbeat went 14 minutes stale while the process
            // sat alive). Exit 4 = idle exit; the reader thread's EOF on the
            // next line feeds Died, and this spawn site's backoff respawns.
            // The plane-S cursor (gap 1) dedupes the respawn's replay.
            const WATCH_IDLE_EXIT_SECS: u64 = 90;
            let mut child = match std::process::Command::new(&toron)
                .args([
                    "watch",
                    "--live",
                    "--project",
                    &project,
                    "--kinds",
                    kinds,
                    "--idle-exit",
                    &WATCH_IDLE_EXIT_SECS.to_string(),
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(_) => {
                    let _ = tx.send(BrainMsg::Died).await;
                    return;
                }
            };
            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    let _ = tx.send(BrainMsg::Died).await;
                    return;
                }
            };
            if let Ok(mut g) = slot.lock() {
                *g = Some(WatchChild(child));
            }
            std::thread::spawn(move || {
                use std::io::BufRead;
                // Heartbeat, throttled to 1s: `/health` reads this mtime so
                // a watcher on the wrong bus (or a dead child) is visible
                // instead of indistinguishable from a quiet workspace.
                let mut stamped: Option<std::time::Instant> = None;
                for line in std::io::BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            if stamped
                                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
                            {
                                crate::watchbeat::touch(&fw_state, &workspace);
                                stamped = Some(std::time::Instant::now());
                            }
                            if tx.blocking_send(BrainMsg::Watch(l)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.blocking_send(BrainMsg::Died);
            });
        });
    }

    fn inject_allowed(&self, _pin: &str) -> bool {
        match &self.generation {
            // Daemon-keyed generation: stale binary refuses wakes.
            Some(record) => !gen_stale(record, &self.daemon_exe),
            None => true,
        }
    }

    fn pins(&self) -> Vec<String> {
        self.index_pins.clone()
    }

    fn pin_npub(&self, pin: &str) -> Option<String> {
        self.npubs.get(pin).cloned()
    }

    fn prev_status(&self, pin: &str) -> Option<String> {
        self.herdr.get(pin).cloned()
    }

    fn sweep_maintenance(&mut self) -> Vec<(String, String)> {
        self.refresh_index();
        // Status poll: diff the herdr list against last-known and tell
        // changes back as HerdrStatus (forwarder topology, poll transport).
        let current = self.poll_herdr();
        let mut changed: Vec<(String, String)> = current
            .iter()
            .filter(|(pin, st)| self.herdr.get(*pin) != Some(*st))
            .map(|(p, s)| (p.clone(), s.clone()))
            .collect();
        changed.sort();
        // Presence rows tick the trusted edge (same file + freshness rule
        // as the embedded projection).
        let now = Self::now_unix();
        let rows = self.presence_rows(&self.index_pins, now);
        if let Some(tx) = &self.tx {
            for (pin, status) in &changed {
                // `try_send`, never `blocking_send`: the daemon-hosted Brain
                // drives the Host from inside its own tokio runtime, where
                // `blocking_send` panics ("Cannot block the current thread
                // from within a runtime") — on the first status change the
                // sweep task died and the hosted workspace went ring-deaf
                // while /health kept reporting `mode=hosted`. A full channel
                // drops the update, which is what the ignored result already
                // meant.
                let _ = tx.try_send(BrainMsg::HerdrStatus {
                    pin: pin.clone(),
                    status: status.clone(),
                });
            }
        }
        rows
    }

    fn drain_role_bounds(&mut self) -> Vec<(String, String)> {
        self.drain_role_bounds()
    }

    fn drain_facts(&mut self) -> Vec<String> {
        self.drain_facts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;

    fn tmp_state(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbrt-fh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("agents")).unwrap();
        d
    }

    fn host_with_registry(name: &str, entries: serde_json::Value) -> (FileHost, PathBuf) {
        let dir = tmp_state(name);
        std::fs::write(
            dir.join("agents").join("proj.json"),
            serde_json::to_string(&entries).unwrap(),
        )
        .unwrap();
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = "true".into();
        cfg.herdr_bin = "true".into();
        cfg.registry_file = Some(dir.join("agents").join("proj.json"));
        (FileHost::new(cfg), dir)
    }

    fn entries() -> serde_json::Value {
        serde_json::json!([
            {"pane_id": "pane-a", "name": "herdr-a", "mail_identity": "PinA"},
            {"pane_id": "pane-b", "name": "herdr-b", "mail_identity": "PinB"}
        ])
    }

    #[test]
    fn registry_key_shapes() {
        assert_eq!(registry_key("proj"), "proj");
        let k = registry_key("/tmp/x/proj");
        assert!(k.starts_with("proj-") && k.len() == 13, "{k}");
        assert_eq!(k, registry_key("/tmp/x/proj"));
    }

    #[test]
    fn resolve_prefers_plain_over_hashed() {
        let dir = tmp_state("res");
        std::fs::write(dir.join("agents").join("proj.json"), "[]").unwrap();
        std::fs::write(dir.join("agents").join("proj-12345678.json"), "[]").unwrap();
        let (path, ambiguous) = resolve_registry_file(&dir, "proj", None);
        assert!(!ambiguous);
        assert_eq!(path.unwrap().file_name().unwrap(), "proj.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_flags_ambiguous_hashed() {
        use std::time::{Duration, SystemTime};
        let dir = tmp_state("amb");
        let old = dir.join("agents").join("proj-11111111.json");
        let new = dir.join("agents").join("proj-22222222.json");
        std::fs::write(&old, "[]").unwrap();
        std::fs::write(&new, "[]").unwrap();
        // bd-ztek: freshest-mtime wins (the live spawn writes at spawn),
        // not first-sorted — pin the mtimes explicitly, never rely on
        // write order (coarse filesystems tie).
        let t0 = SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(t0 - Duration::from_secs(60))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(t0)
            .unwrap();
        let (path, ambiguous) = resolve_registry_file(&dir, "proj", None);
        assert!(ambiguous);
        assert_eq!(path.unwrap().file_name().unwrap(), "proj-22222222.json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_absolute_path_project_finds_hashed_registry() {
        // The daemon-hosting shape (bd-*: parse_host rsplit): flywheel spawn
        // writes `proj-{8hex}.json` (state::state_key hash of the absolute
        // path); the daemon passes the same absolute string as `project`.
        // Matching the RAW string (slashes never appear in a filename) made
        // every absolute-path project's registry invisible — zero pins, no
        // npub resolution, every wake classified to zero targets.
        let dir = tmp_state("abs");
        let abs = "/tmp/scratch/proj";
        let expected = registry_key(abs); // "proj-{8hex}"
        std::fs::write(dir.join("agents").join(format!("{expected}.json")), "[]").unwrap();
        let (path, ambiguous) = resolve_registry_file(&dir, abs, None);
        assert!(!ambiguous, "single hash candidate is unambiguous");
        assert_eq!(
            path.unwrap().file_name().unwrap().to_string_lossy(),
            format!("{expected}.json"),
            "absolute-path project must resolve its hashed registry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_builds_from_registry_without_npubs_offline() {
        let (mut host, _dir) = host_with_registry("t1", entries());
        host.refresh_index();
        assert_eq!(host.pins(), vec!["PinA".to_string(), "PinB".to_string()]);
        assert!(host.pin_npub("PinA").is_none());
        assert_eq!(host.panes.get("pane-a").cloned(), Some("PinA".to_string()));
    }

    fn watch_line(pin: &str, content: &str) -> String {
        serde_json::json!({
            "kind": 9, "id": "e1", "pubkey": "ab",
            "tags": [["p", pin]], "content": content, "created_at": 1
        })
        .to_string()
    }

    #[test]
    fn presence_filter_idle_keeps_busy_urgent_breaks_through() {
        let (mut host, _dir) = host_with_registry("t2", entries());
        host.refresh_index();
        // Idle presence: routine channel ring prompts.
        host.tick(&BrainMsg::Presence {
            pin: "PinA".into(),
            state: "idle".into(),
        });
        // No status known -> "unknown" status -> core QUEUES (not prompt).
        // Set status idle first via HerdrStatus (drain of empty queue).
        host.tick(&BrainMsg::HerdrStatus {
            pin: "PinA".into(),
            status: "idle".into(),
        });
        let fx = host.tick(&BrainMsg::Watch(watch_line("PinA", "hello")));
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::Prompt { pin, .. } if pin == "PinA")),
            "idle presence + idle status prompts: {fx:?}"
        );
        // Busy presence + routine line -> prompt_filtered Audit.
        host.tick(&BrainMsg::Presence {
            pin: "PinA".into(),
            state: "working".into(),
        });
        let fx = host.tick(&BrainMsg::Watch(watch_line("PinA", "hello again")));
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::Audit { kind, .. } if kind == "prompt_filtered")),
            "busy presence holds routine wake: {fx:?}"
        );
    }

    #[test]
    fn ensure_daemon_gen_refreshes_stale_record() {
        // Regression: the gen file is write-once-if-missing, so a daemon
        // started after a binary upgrade kept the OLD record. gen_stale()
        // then refused every inject forever — budget-granted prompts were
        // journaled and silently dropped (zero panes touched). A restart
        // against a NEW binary must re-arm to the new identity.
        let dir = tmp_state("gen");
        std::fs::create_dir_all(dir.join("doorbell")).unwrap();
        let path = dir.join("doorbell").join("w.daemon.gen");
        let stale = serde_json::json!({
            "started_unix": 1,
            "binary_mtime_unix": 1,
            "binary_len": 1,
            "binary_hash": "deadbeefdeadbeefdeadbeefdeadbeef",
        });
        std::fs::write(&path, stale.to_string()).unwrap();
        ensure_daemon_gen(&dir, "w");
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_ne!(
            raw["binary_hash"], "deadbeefdeadbeefdeadbeefdeadbeef",
            "stale daemon gen must refresh to the running binary identity"
        );
        // A matching record is kept (idempotent, preserves started_unix).
        ensure_daemon_gen(&dir, "w");
        let again: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw, again, "matching gen is idempotent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn busy_presence_urgent_breaks_through() {
        let (mut host, _dir) = host_with_registry("t3", entries());
        host.refresh_index();
        host.tick(&BrainMsg::HerdrStatus {
            pin: "PinA".into(),
            status: "idle".into(),
        });
        host.tick(&BrainMsg::Presence {
            pin: "PinA".into(),
            state: "working".into(),
        });
        // Urgent line shape carries a "request-release" marker.
        let line = serde_json::json!({
            "kind": 14, "id": "e9", "pubkey": "ab",
            "tags": [["p", "PinA"], ["toron-kind", "request-release"]],
            "content": "release please", "created_at": 1
        })
        .to_string();
        let fx = host.tick(&BrainMsg::Watch(line));
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::Prompt { pin, .. } if pin == "PinA")),
            "busy + urgent prompts: {fx:?}"
        );
    }

    #[test]
    fn fetch_inbox_normalizes_like_embedded() {
        let (mut host, _dir) = host_with_registry("t4", entries());
        let fx = host.tick(&BrainMsg::AckSlaTick { now: 1_000 });
        assert!(fx.iter().any(|e| matches!(e, Effect::CheckAckSla)));
        // Empty pin/since: the brain fetches per-pin cursors (embedded advisory shape).
        assert!(fx.iter().any(|e| matches!(
            e,
            Effect::FetchInbox { pin, since } if pin.is_empty() && since.is_empty()
        )));
        let fx = host.tick(&BrainMsg::Sweep {
            since: "x".into(),
            now: 1,
        });
        assert_eq!(fx.len(), 1);
        assert!(matches!(
            &fx[0],
            Effect::FetchInbox { pin, since } if pin.is_empty() && since.is_empty()
        ));
    }

    #[test]
    fn role_bound_is_classification_neutral() {
        let (mut host, _dir) = host_with_registry("t5", entries());
        host.refresh_index();
        let fx = host.tick(&BrainMsg::RoleBound {
            role: "coder".into(),
            pin: "PinA".into(),
        });
        assert!(fx.is_empty());
    }

    #[test]
    fn fact_promotes_to_captain_over_idle_presence() {
        // bd-ztek: daemon-side facts (including inject) must ring a bound
        // idle captain exactly like the embedded workspace arm — no more
        // unconditional fact_miss.
        let (mut host, _dir) = host_with_registry("t-fact", entries());
        host.refresh_index();
        host.tick(&BrainMsg::RoleBound {
            role: "captain".into(),
            pin: "PinA".into(),
        });
        host.tick(&BrainMsg::Presence {
            pin: "PinA".into(),
            state: "idle".into(),
        });
        let fx = host.tick(&BrainMsg::Fact {
            summary: "deploy".into(),
        });
        assert_eq!(fx.len(), 1, "captain fact over idle must prompt: {fx:?}");
        match &fx[0] {
            Effect::Prompt { pin, line } => {
                assert_eq!(pin, "PinA");
                assert!(line.contains("[doorbell]"));
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn fact_without_captain_stays_fact_miss() {
        // No bind (or non-idle presence): the loud miss survives.
        let (mut host, _dir) = host_with_registry("t-fact-miss", entries());
        host.refresh_index();
        let fx = host.tick(&BrainMsg::Fact {
            summary: "deploy".into(),
        });
        assert!(
            fx.iter().any(|e| matches!(
                e,
                Effect::Audit { kind, .. } if kind == "fact_miss"
            )),
            "unbound fact must miss loud: {fx:?}"
        );
    }

    #[test]
    fn drains_read_and_remove_pending_files() {
        let (mut host, dir) = host_with_registry("t6", entries());
        std::fs::create_dir_all(dir.join("roles")).unwrap();
        std::fs::create_dir_all(dir.join("facts")).unwrap();
        std::fs::write(
            dir.join("roles").join("proj-pending.jsonl"),
            "{\"role\":\"coder\",\"pin\":\"PinA\"}\nnot json\n{\"role\":\"\",\"pin\":\"x\"}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("facts").join("proj-pending.jsonl"),
            "\"fact one\"\n",
        )
        .unwrap();
        // role key for a bare slug is the slug itself.
        let bounds = host.drain_role_bounds();
        assert_eq!(bounds, vec![("coder".to_string(), "PinA".to_string())]);
        assert!(!dir.join("roles").join("proj-pending.jsonl").exists());
        assert_eq!(host.drain_facts(), vec!["fact one".to_string()]);
        assert!(!dir.join("facts").join("proj-pending.jsonl").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stub_toron_feeds_inbox_and_whois() {
        let dir = tmp_state("stub");
        let script = dir.join("toron");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$2\" = \"whois\" ]; then echo '{\"npub\":\"NPUB1\"}'; else echo '[{\"kind\":9}]'; fi\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::write(
            dir.join("agents").join("proj.json"),
            serde_json::to_string(&entries()).unwrap(),
        )
        .unwrap();
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = script.to_string_lossy().into_owned();
        cfg.herdr_bin = "true".into();
        cfg.registry_file = Some(dir.join("agents").join("proj.json"));
        let mut host = FileHost::new(cfg);
        host.refresh_index();
        assert_eq!(host.pin_npub("PinA").as_deref(), Some("NPUB1"));
        let rows = host.fetch_inbox("PinA", "2026-01-01T00:00:00Z");
        assert_eq!(rows.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The daemon-hosted Brain runs the `Host` on ITS OWN tokio runtime, so
    /// the sweep path must never use `Sender::blocking_send`: it panics
    /// ("Cannot block the current thread from within a runtime"), the sweep
    /// task dies on the first herdr status change, and the hosted workspace
    /// goes ring-deaf while `/health` still says `mode=hosted` (only the
    /// climbing `sweep.last_age_s` and a frozen journal show it).
    #[test]
    fn sweep_forwards_status_changes_without_blocking_a_runtime() {
        let dir = tmp_state("sweep-runtime");
        let script = dir.join("herdr");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"result\":{\"agents\":[{\"pane_id\":\"pane-a\",\"agent_status\":\"working\"}]}}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::write(
            dir.join("agents").join("proj.json"),
            serde_json::to_string(&entries()).unwrap(),
        )
        .unwrap();
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = "true".into();
        cfg.herdr_bin = script.to_string_lossy().into_owned();
        cfg.registry_file = Some(dir.join("agents").join("proj.json"));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut host = FileHost::new(cfg).with_tx(tx);
        host.refresh_index();
        // Same context the daemon's Brain calls it in.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = host.sweep_maintenance();
        });
        assert!(
            rx.try_recv().is_ok(),
            "the herdr status change must reach the brain channel"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_gen_gate() {
        let dir = tmp_state("gen");
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = "true".into();
        cfg.herdr_bin = "true".into();
        let host = FileHost::new(cfg);
        assert!(host.inject_allowed("PinA"), "no gen file allows");
        ensure_daemon_gen(&dir, "w");
        let host2 = FileHost::new(FileHostConfig::new(dir.clone(), "proj", "w"));
        assert!(host2.inject_allowed("PinA"), "matching gen allows");
        // Corrupt the record: stale binary refuses wakes.
        let path = dir.join("doorbell").join("w.daemon.gen");
        let mut rec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        rec["binary_len"] = serde_json::json!(1);
        rec["binary_hash"] = serde_json::json!("deadbeef");
        std::fs::write(&path, serde_json::to_vec(&rec).unwrap()).unwrap();
        let host3 = FileHost::new(FileHostConfig::new(dir.clone(), "proj", "w"));
        assert!(!host3.inject_allowed("PinA"), "stale gen refuses");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

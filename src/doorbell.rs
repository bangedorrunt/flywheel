// governed-by: ADR-0025
// governed-by: ADR-0005
// governed-by: ADR-0011
//! Workspace doorbell (ADR-0005 D1): ring rules, queue coalesce, inject
//! gate. The live dispatcher process is spawn-owned; this module is the
//! functional core the e2e gate tests.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

// ADR-0012 Phase 0: pure core lives in `doorbell-core`; this module re-exports
// it (single source) and keeps the effect layer (pid files, supervision,
// journal, inject). No signature changes in the move.
pub use doorbell_core::{SelfIndex, WakeState, note_wake, parse_status_pair};
// `PinBudget` is consumed live only by the `serve` emit-rate limiter (and by
// this module's tests), so the re-export is gated to those two builds — the
// lean backend-only lane would otherwise carry an unused import.
#[cfg(any(test, feature = "serve"))]
pub use doorbell_core::PinBudget;
// The rest of doorbell-core's surface is exercised only by this module's own
// tests (plus the `#[cfg(test)]` `fanout_now` reference implementation), so the
// re-export is test-gated — in the bin build it would be unused.
#[cfg(test)]
pub use doorbell_core::{
    DoorbellQueue, EventClass, RingInput, UNANSWERED_SECS, classify, doorbell_line,
    inbox_high_water, is_drain_transition, parse_inbox_item, parse_rfc3339_micros,
    parse_watch_ndjson, should_inject, watch_pubkey, watch_targets,
};

/// Pid file: `~/.local/state/flywheel/doorbell/<workspace>.pid`.
pub fn pid_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir.join("doorbell").join(format!("{workspace}.pid"))
}

/// ADR-0005 D1.4 first line: `dispatcher: up|down …`
pub fn dispatcher_status_line(state_dir: &Path, workspace: &str) -> String {
    match inspect_lock(&pid_file(state_dir, workspace)) {
        LockAction::Attach { pid } => format!("dispatcher: up pid={pid}"),
        // inspect_lock is generation-blind; Superseded only arises from
        // inspect_lock_generation, but stay exhaustive and truthful.
        LockAction::Superseded { pid } => format!("dispatcher: up pid={pid} superseded"),
        LockAction::Reloaded { stale } => format!("dispatcher: down stale={stale}"),
        LockAction::Start => "dispatcher: down".into(),
    }
}

/// Helm/snapshot token: `absent` (never started) vs `down` (dead) vs `up`.
/// Snapshot must not treat lowercase `down` as absent.
pub fn dispatcher_token(state_dir: &Path, workspace: &str) -> &'static str {
    let pid_path = pid_file(state_dir, workspace);
    let gen_path = gen_file(state_dir, workspace);
    if !pid_path.exists() && !gen_path.exists() {
        return "absent";
    }
    match inspect_lock(&pid_path) {
        LockAction::Attach { .. } | LockAction::Superseded { .. } => "up",
        LockAction::Reloaded { .. } | LockAction::Start => "down",
    }
}

/// Workspace keys a doorbell might own. Always includes `swarm_label` so
/// arrive-started dispatchers stay visible with an empty spawn registry.
pub fn candidate_workspaces(
    swarm_label: &str,
    spawn_meta: Option<&str>,
    registry_labels: &[String],
) -> Vec<String> {
    let mut keys = std::collections::BTreeSet::new();
    if !swarm_label.is_empty() {
        keys.insert(swarm_label.to_string());
    }
    if let Some(meta) = spawn_meta.filter(|s| !s.is_empty()) {
        keys.insert(meta.to_string());
    }
    for label in registry_labels {
        if !label.is_empty() {
            keys.insert(label.clone());
        }
    }
    keys.into_iter().collect()
}

/// Helm/snapshot workspace: live dispatcher first, then any started
/// lock, then swarm_label. Stale spawn-meta (e.g. `adrx28`) must not hide
/// the arrive-started child.
pub fn preferred_workspace(
    swarm_label: &str,
    spawn_meta: Option<&str>,
    token: impl Fn(&str) -> &'static str,
) -> String {
    let keys = candidate_workspaces(swarm_label, spawn_meta, &[]);
    for label in &keys {
        if token(label) == "up" {
            return label.clone();
        }
    }
    for label in &keys {
        if token(label) != "absent" {
            return label.clone();
        }
    }
    swarm_label.to_string()
}

/// One-line identity prime. Injected even with `--no-peer-aware` (D1 item 1).
pub fn identity_prime(name: &str, mail_identity: &str) -> String {
    format!(
        "You are {name}. Your mail identity is {mail_identity}. Before any spawn: flywheel arrive <project> --as {mail_identity} then flywheel briefing \"<objective>\" --json. Guard will block unattributed/unreserved commits."
    )
}

// `PinBudget` (the per-pin 5/30/3 rolling budget, D1 item 2) is pure core and
// lives in `doorbell-core` — re-exported above with the rest of the core
// surface (ADR-0012 Phase 0: ONE implementation). The plane-P copy that lived
// here carried a never-read `started_unix` and a wall-clock `record()` wrapper;
// both are gone, and callers pass `now` explicitly per the Tick Law.

pub fn read_pid(path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse().ok()
}

pub fn write_pid(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(path, format!("{pid}\n")).with_context(|| format!("write {}", path.display()))
}

pub fn clear_pid(path: &Path) {
    let _ = fs::remove_file(path);
}

pub fn pid_alive(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn kill_pid(pid: u32) {
    if pid > 1 {
        let _ = Command::new("kill")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Watcher generation record (bd-stale-doorbell-wake-0fic). A dispatcher
/// writes this next to its pid file at start; a watcher whose recorded
/// binary identity no longer matches the on-disk binary predates the
/// current wake logic and must not inject wakes.
///
/// Sanctioned restart (operator/agent): rebuild, then re-run the workspace
/// spawn command (`flywheel spawn …`). The next spawn supersedes any live
/// watcher whose generation does not match the new binary; an already
/// running current-binary watcher also self-retires on its 1s tick.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WatcherGen {
    pub started_unix: u64,
    pub binary_mtime_unix: u64,
    pub binary_len: u64,
    /// sha1 of first 8k of the binary (gap 10 fix: same-second rebuild with same len
    /// still supersedes). Empty for pre-hash gens (back-compat).
    #[serde(default)]
    pub binary_hash: String,
}

impl WatcherGen {
    /// Identity snapshot of `exe` right now, stamped with `started_unix`.
    pub fn capture(exe: &Path, started_unix: u64) -> Option<Self> {
        let (binary_mtime_unix, binary_len) = binary_identity(exe)?;
        let binary_hash = binary_hash(exe).unwrap_or_default();
        Some(Self {
            started_unix,
            binary_mtime_unix,
            binary_len,
            binary_hash,
        })
    }

    fn matches(&self, mtime: u64, len: u64) -> bool {
        self.binary_mtime_unix == mtime && self.binary_len == len
    }

    fn hash_matches(&self, hash: &str) -> bool {
        !self.binary_hash.is_empty() && self.binary_hash == hash
    }

    /// True when the on-disk binary differs from what this watcher was
    /// started with — or cannot be read (fail closed): refuse wakes.
    pub fn stale(&self, exe: &Path) -> bool {
        match binary_identity(exe) {
            Some((mtime, len)) => {
                // Prefer hash when we have it (strong), fallback to mtime+len for old gens.
                if !self.binary_hash.is_empty() {
                    match binary_hash(exe) {
                        Some(h) => !self.hash_matches(&h),
                        None => true,
                    }
                } else {
                    !self.matches(mtime, len)
                }
            }
            None => true,
        }
    }

    /// Banner suffix so agents can tell stale-watcher noise from a live
    /// peer need: compare against `flywheel __doorbell --report`, which
    /// prints the same fields for the pid-file owner.
    pub fn stamp(&self) -> String {
        if self.binary_hash.is_empty() {
            format!(
                "[doorbell watcher up={up} bin_mtime={mtime}]",
                up = self.started_unix,
                mtime = self.binary_mtime_unix
            )
        } else {
            format!(
                "[doorbell watcher up={up} bin_mtime={mtime} hash={hash:.8}]",
                up = self.started_unix,
                mtime = self.binary_mtime_unix,
                hash = self.binary_hash
            )
        }
    }
}

/// `<workspace>.gen`, sibling of [`pid_file`].
pub fn gen_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir.join("doorbell").join(format!("{workspace}.gen"))
}

/// On-disk identity of a binary: `(mtime unix secs, length bytes)`. Cheap
/// and monotone under rebuilds; answers "did the file under me change"
/// without hashing tens of MB every tick.
pub fn binary_identity(exe: &Path) -> Option<(u64, u64)> {
    let md = fs::metadata(exe).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((mtime, md.len()))
}

/// sha1 of first 8k of the binary (gap 10 hash fix).
pub fn binary_hash(exe: &Path) -> Option<String> {
    use sha1::{Digest, Sha1};
    use std::io::Read;
    let mut f = fs::File::open(exe).ok()?;
    let mut buf = [0u8; 8192];
    let n = f.read(&mut buf).ok()?;
    let mut hasher = Sha1::new();
    hasher.update(&buf[..n]);
    Some(hex::encode(hasher.finalize()))
}

/// Inject-gate for the tick loop: stale watchers refuse all wakes.
pub fn inject_allowed(record: &WatcherGen, exe: &Path) -> bool {
    !record.stale(exe)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn write_gen(path: &Path, record: &WatcherGen) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_vec(record)?)
        .with_context(|| format!("write {}", path.display()))
}

pub fn read_gen(path: &Path) -> Option<WatcherGen> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Append the watcher generation to an injected line so agents can tell a
/// stale-watcher banner from a live one (compare with `__doorbell --report`).
pub fn stamp_line(msg: &str, record: &WatcherGen) -> String {
    format!("{msg} {}", record.stamp())
}

/// What spawn should do with the workspace lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockAction {
    Start,
    Attach {
        pid: u32,
    },
    Reloaded {
        stale: u32,
    },
    /// Live pid whose generation is missing or predates the current
    /// binary: it runs pre-fix wake code and must be replaced (a pre-fix
    /// watcher can never retire itself — the retire logic does not exist
    /// in its binary).
    Superseded {
        pid: u32,
    },
}

pub fn inspect_lock_with(path: &Path, alive: impl Fn(u32) -> bool) -> LockAction {
    match read_pid(path) {
        None => LockAction::Start,
        Some(pid) if alive(pid) => LockAction::Attach { pid },
        Some(pid) => LockAction::Reloaded { stale: pid },
    }
}

pub fn inspect_lock(path: &Path) -> LockAction {
    inspect_lock_with(path, pid_alive)
}

/// Generation-aware lock decision (bd-stale-doorbell-wake-0fic). Attach
/// requires BOTH a live pid and a recorded generation matching
/// `current_identity`; anything alive otherwise is
/// [`LockAction::Superseded`]. Hash-aware: when the recorded gen carries a
/// hash, the on-disk binary hash must match too (same-second rebuild guard).
#[cfg_attr(not(test), allow(dead_code))] // exe-less callers: mtime+len only (see the hash-aware door)
pub fn inspect_lock_generation(
    path: &Path,
    gen_path: &Path,
    current_identity: Option<(u64, u64)>,
    alive: impl Fn(u32) -> bool,
) -> LockAction {
    match read_pid(path) {
        None => LockAction::Start,
        Some(pid) if !alive(pid) => LockAction::Reloaded { stale: pid },
        Some(pid) => match (read_gen(gen_path), current_identity) {
            (Some(record), Some((mtime, len))) => {
                let mtime_ok = record.matches(mtime, len);
                let hash_ok = if record.binary_hash.is_empty() {
                    true
                } else {
                    // Need to check hash against current binary; gen_path's workspace
                    // dir is sibling of pid file, but we don't have exe path here.
                    // Fallback: if we have a gen without exe, rely on mtime only.
                    // A caller holding the exe uses the hash-aware door below.
                    true
                };
                if mtime_ok && hash_ok {
                    LockAction::Attach { pid }
                } else {
                    LockAction::Superseded { pid }
                }
            }
            _ => LockAction::Superseded { pid },
        },
    }
}

/// Hash-aware variant that also checks the binary sha1 when the record carries
/// it — the door `ensure_running` uses, since it holds the exe (gap 10 fix: a
/// same-second rebuild with the same length still supersedes).
pub fn inspect_lock_generation_with_hash(
    path: &Path,
    gen_path: &Path,
    current_identity: Option<(u64, u64)>,
    current_hash: Option<String>,
    alive: impl Fn(u32) -> bool,
) -> LockAction {
    match read_pid(path) {
        None => LockAction::Start,
        Some(pid) if !alive(pid) => LockAction::Reloaded { stale: pid },
        Some(pid) => match (read_gen(gen_path), current_identity) {
            (Some(record), Some((mtime, len))) => {
                let mtime_ok = record.matches(mtime, len);
                let hash_ok = if record.binary_hash.is_empty() {
                    true
                } else {
                    current_hash
                        .as_deref()
                        .map(|h| record.binary_hash == h)
                        .unwrap_or(false)
                };
                if mtime_ok && hash_ok {
                    LockAction::Attach { pid }
                } else {
                    LockAction::Superseded { pid }
                }
            }
            _ => LockAction::Superseded { pid },
        },
    }
}

/// Kill the dispatcher and drop the pid + generation files (ADR-0005 D1).
pub fn stop(state_dir: &Path, workspace: &str) {
    let path = pid_file(state_dir, workspace);
    if let Some(pid) = read_pid(&path) {
        kill_pid(pid);
    }
    clear_pid(&path);
    let _ = fs::remove_file(unanswered_file(state_dir, workspace));
    let _ = fs::remove_file(gen_file(state_dir, workspace));
}

/// Cutover registration (bd-asr2.13, Phase 3): spawn records one
/// subscription row per pinned crew member (herdr sink, own workspace —
/// local-actor allowed, D6). Best-effort: a store failure warns loudly and
/// never fails the spawn. The daemon auto-hosts these rows (bd-2o1v) — it
/// needs the project, so spawn names it. `kill` drops the spawn's own rows
/// (lifecycle symmetry, bd-2o1v); operator/cli rows outlive swarms.
pub fn register_workspace_subscriptions(
    state_dir: &Path,
    project: &str,
    workspace: &str,
    pins: &[String],
) {
    use doorbell_rt::subscribe::{
        Actor, Budget, Filter, Identity, Scope, SinkKind, Subscription, SubscriptionStore,
    };
    let mut store = SubscriptionStore::open(state_dir);
    for pin in pins {
        let sub = Subscription {
            id: format!("spawn-{workspace}-{pin}"),
            identity: Identity::Pin(pin.clone()),
            scope: Scope::One(workspace.to_string()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
            project: Some(project.to_string()),
            endpoint: None,
        };
        if let Err(e) = store.subscribe(Actor::local_process(), sub, workspace) {
            eprintln!("flywheel: cannot record wake subscription for {pin}: {e:?}");
        }
    }
}

/// Arrival registration (bd-kj2z): `arrive` records one operator row for
/// the arriving pin so zero-fleet sessions get woken with no babysitting —
/// the daemon auto-hosts the named pair within 60s. Idempotent per
/// (workspace, pin) — re-arriving refreshes the same row. Best-effort and
/// never fatal to the session door. Stale pins age out via the daemon
/// reaper; `kill` never touches cli rows (no panes to tie them to).
pub fn register_arrival_subscription(state_dir: &Path, project: &str, workspace: &str, pin: &str) {
    use doorbell_rt::subscribe::{
        Actor, Budget, Filter, Identity, Scope, SinkKind, Subscription, SubscriptionStore,
    };
    let mut store = SubscriptionStore::open(state_dir);
    let sub = Subscription {
        id: format!("cli-{workspace}-{pin}"),
        identity: Identity::Pin(pin.to_string()),
        scope: Scope::One(workspace.to_string()),
        filter: Filter::default(),
        sink: SinkKind::Herdr,
        budget: Budget::default_530(),
        project: Some(project.to_string()),
        endpoint: None,
    };
    if let Err(e) = store.subscribe(Actor::local_process(), sub, workspace) {
        eprintln!("flywheel: cannot record arrival wake subscription for {pin}: {e:?}");
    }
}

/// bd-kj2z: arrival rows are idempotent per (workspace, pin) and carry
/// the project for auto-host; re-arriving refreshes the same row,
/// never a second one.
#[test]
fn arrival_subscription_is_idempotent_and_names_project() {
    use doorbell_rt::subscribe::*;
    let dir = std::env::temp_dir().join(format!("dbll-arrive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    register_arrival_subscription(&dir, "proj", "wA", "P");
    register_arrival_subscription(&dir, "proj", "wA", "P");
    let store = SubscriptionStore::open(&dir);
    let rows = store.list();
    assert_eq!(rows.len(), 1, "re-arrive refreshes, never duplicates");
    assert_eq!(rows[0].id, "cli-wA-P");
    assert_eq!(rows[0].project.as_deref(), Some("proj"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cron doorbell sink (bd-asr2.16, D7, flag-gated): on tick completion,
/// journal a Fact row and queue a pending wake. Digest-visible with zero
/// swarm (the embedded Brain ticks drains on sweep); the emit/deliver
/// binding for direct human push lands in bd-asr2.17. Testable core
/// ([`cron_tick_wake_in`]) takes explicit dirs.
pub fn cron_tick_wake(project: &str, cron: &str, tick: &str, outcome: &str) {
    if !crate::config::Config::load()
        .ok()
        .map(|c| c.doorbell_cron_sink())
        .unwrap_or(false)
    {
        return;
    }
    let summary = format!("cron {cron} tick {tick} {outcome}");
    cron_tick_wake_in(&crate::config::state_dir(), project, &summary);
}

pub fn cron_tick_wake_in(state_dir: &Path, project: &str, summary: &str) {
    let msg = crate::workspace::Msg::Fact {
        summary: summary.to_string(),
    };
    journal_append_workspace(
        state_dir,
        project,
        &msg,
        &[],
        serde_json::json!({ "stage": "cron_tick", "summary": summary, "workspace": project, "corr": None::<String> }),
        None,
    );
    if let Err(e) = crate::append_pending_fact(&crate::config::state_dir(), project, summary) {
        eprintln!("flywheel: cron doorbell sink: pending queue failed: {e:#}");
    }
}

/// Tokened service emit (bd-asr2.17, D7): pure authorize step for
/// POST /api/v1/emit. Token compared as hex sha256 (constant-length
/// compare); `to` must match the service channel scope (`*` = any).
/// Revocation = deleting the service entry (then UnknownService).
#[derive(Debug, PartialEq, Eq)]
pub enum EmitDeny {
    UnknownService,
    BadToken,
    ScopeDenied,
}

pub fn authorize_emit(
    services: &std::collections::HashMap<String, crate::config::ServiceEmit>,
    service: &str,
    token: &str,
    to: &str,
) -> Result<u32, EmitDeny> {
    use sha2::{Digest, Sha256};
    let svc = services.get(service).ok_or(EmitDeny::UnknownService)?;
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let hex = format!("{:x}", h.finalize());
    if hex.len() != svc.token_sha256.len()
        || !hex
            .bytes()
            .zip(svc.token_sha256.bytes())
            .fold(0u8, |a, (x, y)| a | (x ^ y))
            .eq(&0)
    {
        return Err(EmitDeny::BadToken);
    }
    if !svc.channels.iter().any(|c| c == "*" || c == to) {
        return Err(EmitDeny::ScopeDenied);
    }
    Ok(svc.rate_per_min.max(1))
}

/// Journal + queue the admitted emit as a Fact row (stage `emit`).
pub fn service_emit_wake_in(state_dir: &Path, project: &str, service: &str, to: &str) {
    let summary = format!("emit {service} -> {to}");
    let msg = crate::workspace::Msg::Fact {
        summary: summary.clone(),
    };
    journal_append_workspace(
        state_dir,
        project,
        &msg,
        &[],
        serde_json::json!({ "stage": "emit", "service": service, "to": to, "workspace": project, "corr": None::<String> }),
        None,
    );
    if let Err(e) = crate::append_pending_fact(&crate::config::state_dir(), project, &summary) {
        eprintln!("flywheel: service emit: pending queue failed: {e:#}");
    }
}

/// Operator console inject (D8, bd-woha): journal a Fact row (stage
/// `inject`) into the named workspace scope plus queue one pending wake —
/// digest-visible with zero swarm. Same-second ratchet (identical pin+line
/// within one unix second refuses) and a persisted per-pin 5/30/3 rolling
/// budget (the CLI is one-shot; RAM cannot hold the window). The console
/// IS the operator (local loopback invocation), so no separate token.
#[derive(Debug, PartialEq, Eq)]
pub enum InjectDeny {
    Ratchet,
    Budget,
}

fn inject_budget_path(state_dir: &Path, project: &str) -> PathBuf {
    state_dir.join(format!(
        "inject-budget-{}.json",
        crate::state::state_key(project)
    ))
}

fn inject_budget_load(
    state_dir: &Path,
    project: &str,
) -> std::collections::BTreeMap<String, Vec<u64>> {
    std::fs::read_to_string(inject_budget_path(state_dir, project))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn inject_wake_in(
    state_dir: &Path,
    project: &str,
    workspace: &str,
    pin: &str,
    line: &str,
    now_unix: u64,
) -> Result<String, InjectDeny> {
    // Same-second ratchet: scan the scope journal tail for an identical
    // pin+line stamped this same second.
    let journal_path = doorbell_rt::FileStore::new(state_dir).journal_file(workspace);
    if let Ok(raw) = std::fs::read_to_string(&journal_path) {
        for row in raw.lines().rev().take(32) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(row) else {
                break;
            };
            let msg = v.get("msg");
            let summary = msg
                .and_then(|m| {
                    m.get("summary")
                        .or_else(|| m.get("Fact").and_then(|f| f.get("summary")))
                })
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let same_pin = summary.contains(pin) && summary.contains(line);
            let same_sec = v
                .get("executed")
                .and_then(|e| e.get("inject_ts"))
                .and_then(|t| t.as_u64())
                .is_some_and(|t| t == now_unix);
            if same_pin && same_sec {
                return Err(InjectDeny::Ratchet);
            }
        }
    }
    let mut budgets = inject_budget_load(state_dir, project);
    let stamps = budgets.entry(pin.to_string()).or_default();
    stamps.retain(|t| now_unix.saturating_sub(*t) < 1800);
    if stamps.len() >= 3 {
        return Err(InjectDeny::Budget);
    }
    stamps.push(now_unix);
    let budget_path = inject_budget_path(state_dir, project);
    if let Some(parent) = budget_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(
        budget_path,
        serde_json::to_string(&budgets).unwrap_or_default(),
    )
    .is_err()
    {
        return Err(InjectDeny::Budget);
    }
    let summary = format!("inject {pin}: {line}");
    let msg = crate::workspace::Msg::Fact {
        summary: summary.clone(),
    };
    journal_append_workspace(
        state_dir,
        workspace,
        &msg,
        &[],
        serde_json::json!({ "stage": "inject", "pin": pin, "workspace": workspace, "inject_ts": now_unix }),
        None,
    );
    let _ = crate::append_pending_fact(&crate::config::state_dir(), project, &summary);
    Ok(summary)
}

fn spawn_child(
    exe: &Path,
    project: &str,
    workspace: &str,
    path: &Path,
    gen_path: &Path,
) -> Result<()> {
    // bd-durable-doorbell-lngf: record which project owns this workspace —
    // the Resonate supervisor reads the sidecar to re-spawn the right
    // `__doorbell --project <p>` after a crash (the pid/gen files alone
    // don't carry it).
    if let Some(parent) = gen_path.parent() {
        let _ = fs::write(parent.join(format!("{workspace}.project")), project);
    }
    let mut cmd = Command::new(exe);
    cmd.args(["__doorbell", "--project", project, "--workspace", workspace]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn doorbell for {workspace}"))?;
    write_pid(path, child.id())?;
    // Generation stamp at spawn (bd-stale-doorbell-wake-0fic): the tick
    // loop compares this record against `exe` every second and retires the
    // watcher when the binary changes underneath it.
    if let Some(record) = WatcherGen::capture(exe, unix_now()) {
        let _ = write_gen(gen_path, &record);
    }
    Ok(())
}

/// Start or attach the detached `__doorbell` child. Never forks a second
/// live process for the same workspace lock — and never attaches to a
/// watcher whose generation predates `exe` (bd-stale-doorbell-wake-0fic):
/// a live pre-fix watcher is killed and replaced by a fresh child.
pub fn ensure_running(
    exe: &Path,
    state_dir: &Path,
    workspace: &str,
    project: &str,
) -> Result<LockAction> {
    // D9 layer 3 (bd-asr2.5): a healthy daemon owns wake — attach as its
    // client, never spawn a local loop (no double prompts). Subscription
    // registration via API lands in bd-asr2.8; until then this is a loud
    // attach with no local writer. Stop local dispatchers before enabling
    // the daemon host (`flywheel kill`), else both ring (Phase 3 ends this).
    let cfg_url = crate::config::Config::load()
        .ok()
        .and_then(|c| c.doorbell_url());
    // bd-0gmw: a configured daemon that is down gets one pidfile-mode
    // bring-up attempt (never launchctl bootstrap — persistence stays
    // opt-in via `doorbell install`) before host resolution degrades us.
    // Attached/Started → resolve_host below sees /health and defers;
    // Degraded → fall through, embedded as today.
    if cfg_url.is_some()
        && let crate::daemon::DoorbellEnsure::Degraded { hint } =
            crate::daemon::ensure_doorbell_spawn()
    {
        eprintln!("flywheel doorbell: {hint}");
    }
    if matches!(
        resolve_host(state_dir, cfg_url, Some((project, workspace))),
        DoorbellHost::Proxy { .. }
    ) {
        eprintln!(
            "flywheel doorbell: daemon host alive — attached, local loop suppressed (deference)"
        );
        return Ok(LockAction::Attach { pid: 0 });
    }
    let path = pid_file(state_dir, workspace);
    let gen_path = gen_file(state_dir, workspace);
    // Hash-aware: this caller holds `exe`, so the sha1 short-circuit of gap 10
    // applies (mtime+len alone cannot tell a same-second rebuild apart).
    let action = inspect_lock_generation_with_hash(
        &path,
        &gen_path,
        binary_identity(exe),
        binary_hash(exe),
        pid_alive,
    );
    match action {
        LockAction::Attach { .. } => Ok(action),
        LockAction::Superseded { pid } => {
            kill_pid(pid);
            clear_pid(&path);
            let _ = fs::remove_file(&gen_path);
            spawn_child(exe, project, workspace, &path, &gen_path)?;
            Ok(action)
        }
        LockAction::Start | LockAction::Reloaded { .. } => {
            if matches!(action, LockAction::Reloaded { .. }) {
                clear_pid(&path);
            }
            spawn_child(exe, project, workspace, &path, &gen_path)?;
            Ok(action)
        }
    }
}

/// Host resolution (ADR-0012 D1/D9): explicit `[doorbell] url` whose
/// `/health` answers selects the daemon; otherwise a 200ms loopback probe
/// defers to a running daemon, else embedded in-process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoorbellHost {
    Embedded,
    Proxy { url: String },
}

/// `target` is `Some((project, workspace))` whenever the caller has one.
/// It is load-bearing, not decoration: deference SUPPRESSES the local loop,
/// so deferring on "a daemon answered" alone left any workspace the daemon
/// does not host with no writer and no rings — silently deaf. The probe is
/// now qualified by (a) the daemon actually hosting this workspace and
/// (b) this workspace's OWN lease, never a stranger's on another one.
/// `None` asserts neither and is for generic lifecycle verbs (status/stop)
/// that address the daemon itself rather than a workspace.
pub fn resolve_host(
    state_dir: &Path,
    cfg_url: Option<String>,
    target: Option<(&str, &str)>,
) -> DoorbellHost {
    use doorbell_rt::lease::{deference_decision, live_leases};
    const DEFAULT_URL: &str = "http://127.0.0.1:18980";
    let now = doorbell_rt::lease::unix_now();
    // Per-workspace lease probe: leases are per workspace (bd-ene2), so a
    // live lease on another workspace must never refuse THIS one.
    let probe = target.and_then(|(_, ws)| {
        live_leases(state_dir, now)
            .into_iter()
            .find(|(w, _)| w == ws)
            .map(|(_, lease)| lease)
    });
    let resolve_at = |url: &str, configured: bool| -> DoorbellHost {
        // bd-2zqk: keep the failure so the warning can name the real cause.
        // A busy-but-alive daemon must not be reported as unreachable.
        let (health, health_err) = match daemon_health_probe(url) {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let hosts_target = match (&health, target) {
            (Some(v), Some((p, w))) => daemon_hosts(v, p, w),
            (Some(_), None) => true,
            (None, _) => false,
        };
        // Up but not hosting us is a real, common state (standby daemon,
        // or a daemon armed for other workspaces). Name it and run local.
        if let (Some(v), Some((p, w))) = (&health, target)
            && !daemon_hosts(v, p, w)
        {
            let mode = v.get("mode").and_then(|m| m.as_str()).unwrap_or("unknown");
            eprintln!(
                "flywheel doorbell: daemon {url} is alive (mode={mode}) but does not host {p}/{w} — not deferring; running the local loop (arm it: doorbell start --host {p}/{w})"
            );
        }
        match deference_decision(probe.as_ref(), hosts_target, now) {
            doorbell_rt::lease::Deference::DeferToDaemon => DoorbellHost::Proxy {
                url: url.to_string(),
            },
            doorbell_rt::lease::Deference::TakeLease => {
                if configured && health.is_none() {
                    // bd-2zqk: say WHICH failure. "unreachable" plus "start a
                    // daemon" was actively wrong for the two cases that matter
                    // most — a busy daemon and a version mismatch — because the
                    // daemon was already running.
                    let cause = health_err.map_or("unknown", |e| e.reason());
                    let remediation = match health_err {
                        Some(HealthProbeFailure::NotListening) | None => {
                            "remediation: doorbell start | doorbell install | flywheel daemon start"
                        }
                        // Already listening: starting another daemon is wrong.
                        Some(_) => "no restart needed; re-probe, or check the daemon's version",
                    };
                    eprintln!(
                        "flywheel doorbell: WARNING configured daemon {url} health probe failed: \
                         {cause} — {remediation} — embedded degraded mode"
                    );
                }
                DoorbellHost::Embedded
            }
            doorbell_rt::lease::Deference::RefuseDoubleRun => {
                eprintln!(
                    "flywheel doorbell: live lease held by another writer — refusing second loop (see doorbell/owner-*.json)"
                );
                DoorbellHost::Embedded
            }
        }
    };
    if let Some(url) = cfg_url {
        return resolve_at(&url, true);
    }
    resolve_at(DEFAULT_URL, false)
}

/// Is the daemon at `url` hosting `project/workspace`? `hosted` is the
/// `/health` list of `project/workspace` pairs the daemon writes for.
fn daemon_hosts(v: &serde_json::Value, project: &str, workspace: &str) -> bool {
    let target = format!("{project}/{workspace}");
    v.get("hosted")
        .and_then(|h| h.as_array())
        .is_some_and(|a| a.iter().any(|e| e.as_str() == Some(target.as_str())))
}

/// 200ms loopback probe: `/health` answers with our protocol.
pub fn daemon_healthy(url: &str) -> bool {
    daemon_health_json(url).is_some()
}

/// The parsed `/health` body, or `None` when nothing doorbell-shaped
/// answers. Parsed (not substring-matched) so callers can ask WHAT the
/// daemon hosts — "answers" and "does our work" are different questions.
/// bd-2zqk: why the health probe came back empty. Every arm used to collapse
/// into a single `None`, which the caller reported as "unreachable" — so a
/// healthy daemon under load, a drifted payload, and a protocol bump were all
/// reported as "the daemon is not there", with a remediation that says to start
/// a daemon which is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthProbeFailure {
    /// The URL had no host:port we could dial.
    UnparseableUrl,
    /// Nothing accepted the connection.
    NotListening,
    /// Accepted, then went quiet past the read budget.
    ReadTimedOut,
    /// Answered, but not with the JSON we asked for.
    BadPayload,
    /// Answered with valid JSON that is not doorbell.v1.
    ProtocolMismatch,
}

impl HealthProbeFailure {
    /// The operator-facing reason. Never say "unreachable" for a daemon that
    /// answered — that sends them to start a second daemon.
    pub fn reason(self) -> &'static str {
        match self {
            Self::UnparseableUrl => "the configured url has no dialable host:port",
            Self::NotListening => "nothing is listening on that address",
            Self::ReadTimedOut => {
                "the daemon accepted the connection but did not answer /health within the budget \
                 (it is busy; this is NOT a dead daemon — re-probe, do not start a second one)"
            }
            Self::BadPayload => "the daemon answered /health with a non-JSON body",
            Self::ProtocolMismatch => {
                "the daemon answered but is not doorbell.v1 — a version mismatch, not an \
                 unreachable daemon"
            }
        }
    }
}

/// bd-2zqk: probe budgets.
///
/// The old 200ms connect / 500ms read looked generous and was not. MEASURED on
/// the live daemon at `doorbell serve --port 18980`: /health returns a ~1.4KB
/// JSON body with latency min 0.4ms, median 1.0ms, and a **506ms tail** — the
/// tail sits right on the old 500ms read budget. Over 12 probes the old budgets
/// reported **4 of 12 healthy daemons as "unreachable"** (a 33% false-negative
/// rate, which also pushed the session into `embedded degraded mode` and printed
/// a remediation to start a daemon that was already running). At 1s/2s the same
/// 12 probes were 12/12 clean. The daemon is busy sweeping, so the tail is real
/// and the budget has to clear it.
const HEALTH_CONNECT_TIMEOUT_MS: u64 = 1_000;
const HEALTH_READ_TIMEOUT_MS: u64 = 2_000;

/// Probe the daemon's `/health`, distinguishing every failure mode.
pub fn daemon_health_probe(url: &str) -> Result<serde_json::Value, HealthProbeFailure> {
    let health_url = format!("{url}/health");
    let addr = health_url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    let sock = addr
        .parse::<std::net::SocketAddr>()
        .or_else(|_| format!("{addr}:80").parse::<std::net::SocketAddr>())
        .map_err(|_| HealthProbeFailure::UnparseableUrl)?;
    let mut stream = std::net::TcpStream::connect_timeout(
        &sock,
        std::time::Duration::from_millis(HEALTH_CONNECT_TIMEOUT_MS),
    )
    .map_err(|_| HealthProbeFailure::NotListening)?;
    use std::io::{Read, Write};
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(
        HEALTH_READ_TIMEOUT_MS,
    )));
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_| HealthProbeFailure::NotListening)?;
    let mut raw = String::new();
    // A read timeout here means the daemon IS there and busy. That is the whole
    // point of the distinction: it must never be reported as unreachable.
    stream
        .read_to_string(&mut raw)
        .map_err(|_| HealthProbeFailure::ReadTimedOut)?;
    let payload = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or(&raw);
    let v: serde_json::Value =
        serde_json::from_str(payload).map_err(|_| HealthProbeFailure::BadPayload)?;
    if v.get("protocol").and_then(|p| p.as_str()) != Some("doorbell.v1") {
        return Err(HealthProbeFailure::ProtocolMismatch);
    }
    Ok(v)
}

fn daemon_health_json(url: &str) -> Option<serde_json::Value> {
    daemon_health_probe(url).ok()
}
/// Fan-out: given classified rings and pane status, produce (pin, line)
/// pairs that should be prompted now. Working pins are skipped (queued
/// by the caller). Plane-P reference implementation — the live path is the
/// doorbell-rt Brain's `deliver_prompts`; this stays as the tested statement
/// of the ring/status rule.
#[cfg(test)]
pub fn fanout_now(
    rings: &[(String, EventClass, String)],
    status_by_pin: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (pin, class, hint) in rings {
        let status = status_by_pin.get(pin).map(String::as_str).unwrap_or("idle");
        if !should_inject(status) {
            continue;
        }
        out.push((pin.clone(), doorbell_line(*class, hint)));
    }
    out
}

// bd-drub: the heard-or-deaf tracker lives in ONE place now —
// `doorbell_rt::HearTracker` (byte-identical to the struct + impl that stood
// here). Only the tests still reach it through this path, so the re-export is
// test-gated: in the bin build it would be an unused import.
#[cfg(test)]
pub use doorbell_rt::HearTracker;

pub fn unanswered_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("{workspace}.unanswered"))
}

pub fn read_unanswered(path: &Path) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn wake_state_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir.join(format!("doorbell-wake-{workspace}.json"))
}

/// Missing or corrupt file → DEFAULT (never a crash): at-least-once
/// degrades to re-delivery, which the `[doorbell]` line makes idempotent.
pub fn load_wake_state(path: &Path) -> WakeState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_wake_state(path: &Path, st: &WakeState) -> Result<()> {
    let json = serde_json::to_string(st)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)?;
    Ok(())
}

// ---- bd-durable-doorbell-lngf: Resonate-primary supervision --------------
// The lazily-healed watcher is the LOW-LATENCY push path; the Resonate
// schedule is the GUARANTEED path: the host server fires the supervisor
// tick every minute whether or not any human touches flywheel. A tick that
// lands while no worker is connected stays queued server-side and executes
// at-least-once as soon as any flywheel process with this function
// registered comes alive (the `[doorbell]` line keeps redelivery
// idempotent). Server unreachable → the lazy fallback covers every
// interactive invocation (subtract-before-you-add: no new daemon).

/// Sidecar: which project owns a workspace dispatcher (written by
/// `spawn_child`; the pid/gen files alone don't carry it).
pub fn project_sidecar(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("{workspace}.project"))
}

pub fn read_project_sidecar(state_dir: &Path, workspace: &str) -> Option<String> {
    fs::read_to_string(project_sidecar(state_dir, workspace))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Every dispatcher workspace whose pid is missing or dead: (label, project).
/// Workspaces without a sidecar are skipped — they predate the supervisor.
pub fn dead_dispatchers(state_dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(state_dir.join("doorbell")) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("gen") {
            continue;
        }
        let Some(label) = path.file_stem().and_then(|x| x.to_str()) else {
            continue;
        };
        let alive = read_pid(&pid_file(state_dir, label)).is_some_and(pid_alive);
        if !alive && let Some(project) = read_project_sidecar(state_dir, label) {
            out.push((label.to_string(), project));
        }
    }
    out
}

/// The durable supervisor leaf: heal every dead dispatcher. Checkpointed —
/// a replay returns the stored result and never double-spawns a child.
// resonate_sdk::function requires an args parameter; heal takes no input.
// Item-level expect/allow cannot cover it: the macro regenerates the
// function, so the lint fires on the emitted copy, not this node (see the
// `no_effect_underscore_binding` allow in [lints.clippy]).
#[resonate_sdk::function]
async fn heal_tick(_args: ()) -> Result<Vec<String>> {
    let state_dir = crate::config::state_dir();
    let exe = std::env::current_exe()?;
    let mut healed = Vec::new();
    for (label, project) in dead_dispatchers(&state_dir) {
        if let Err(e) = ensure_running(&exe, &state_dir, &label, &project) {
            return Err(resonate_sdk::error::Error::Application {
                message: format!("supervisor heal {label}: {e:#}"),
            });
        }
        healed.push(label.clone());
    }
    Ok(healed)
}

/// Register the supervisor leaf on a Resonate instance (the schedule fires
/// `heal_tick`; see SUPERVISOR_CRON).
pub fn supervisor_function_registered(r: &mut resonate_sdk::prelude::Resonate) -> Result<()> {
    r.register(heal_tick)
        .map_err(|e| anyhow::anyhow!("register doorbell heal_tick: {e}"))
}

/// The cron the schedule fires on: every minute.
pub const SUPERVISOR_CRON: &str = "* * * * *";
pub const SUPERVISOR_NAME: &str = "doorbell-supervisor";

pub fn report_text(state_dir: &Path, workspace: &str) -> String {
    let head = dispatcher_status_line(state_dir, workspace);
    let gen_tail = match read_gen(&gen_file(state_dir, workspace)) {
        Some(record) => format!(" {}", record.stamp()),
        None => String::new(),
    };
    let pins = read_unanswered(&unanswered_file(state_dir, workspace));
    let head = format!("{head}{gen_tail}");
    if pins.is_empty() {
        format!("{head}\ndoorbell: ok")
    } else {
        format!("{head}\ndoorbell: unanswered pin={}", pins.join(","))
    }
}

/// Gap 20: audit JSONL per decision — why each wake fired.
pub fn audit_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("{workspace}.audit.jsonl"))
}

pub fn audit_append(state_dir: &Path, workspace: &str, entry: serde_json::Value) {
    use std::io::Write;
    let path = audit_file(state_dir, workspace);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        line.push('\n');
        let _ = f.write_all(line.as_bytes());
    }
}

// Pins addressed by this watch line: `p`/`assignee` (pin or npub) plus
// kind-9 `@pin` in content (live posts do both; `p` is npub).

// Second reducers deleted per bd-3wzl.
// Live dispatcher now routes every watch/status edge through `workspace::tick` (tick(&WorkspaceModel, &Msg)).
/// Tests below port the same scenarios to `workspace::tick` (see `sc_d1_*`).
/// herdr-mock: stdin watch lines + pin statuses → JSON injects via tick (no second reducer).
pub fn once_json(stdin: &str, status_by_pin: &BTreeMap<String, String>) -> String {
    let index = SelfIndex::from_pins(status_by_pin.keys().cloned());
    let mut model = crate::workspace::WorkspaceModel::default();
    model.doorbell.index = index;
    for (pin, status) in status_by_pin {
        let (next, _) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::HerdrStatus {
                pin: pin.clone(),
                status: status.clone(),
            },
        );
        model = next;
    }
    let mut rows = Vec::new();
    for line in stdin.lines() {
        let msg = crate::workspace::Msg::WatchNdjson {
            line: line.to_string(),
        };
        let (next, cmds) = crate::workspace::tick(&model, &msg);
        model = next;
        for a in cmds {
            if let crate::workspace::Cmd::Prompt { pin, line } = a {
                rows.push(serde_json::json!({"pin": pin, "line": line}));
            }
        }
    }
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
}

// ---- bd-yw4s: doorbell journal — the `doorbell why` trust surface --------

/// Workspace-tick journal entry writer. `pins` is the inject-relevant pin set
/// (bd-z54w honesty contract): the fold restores the SelfIndex from it so
/// classification reproduces deterministically. `None` (or entries predating
/// the field) fold with Unknown-presence semantics — the fold never invents
/// idle. Serde-side the field is plain JSON (`null` when absent); readers use
/// `entry.get("pins")` with missing/null meaning unknown.
/// Append via the doorbell-rt store (bd-asr2.4): rotation, seq-capable
/// envelope, one implementation. Legacy writer (no seq): the fold
/// tolerates seq-less rows without a contiguity check.
pub fn journal_append_workspace(
    state_dir: &Path,
    workspace: &str,
    msg: &crate::workspace::Msg,
    cmds: &[crate::workspace::Cmd],
    executed: serde_json::Value,
    pins: Option<&[String]>,
) {
    let store = doorbell_rt::FileStore::new(state_dir);
    let msg_v = serde_json::to_value(msg).unwrap_or(serde_json::Value::Null);
    let actions_v = serde_json::to_value(cmds).unwrap_or(serde_json::Value::Null);
    let _ = store.append_journal(workspace, &msg_v, &actions_v, executed, pins.unwrap_or(&[]));
}

/// Journal envelope version (ADR-0012 D4.1, Phase 0). Writers stamp `"v": 1`;
/// readers treat a missing `v` as 1 (pre-v1 lines replay unchanged) and
/// refuse entries whose `v` they do not understand — loudly, never fold-past.
pub fn journal_entry_version(entry: &serde_json::Value) -> u64 {
    entry.get("v").and_then(|v| v.as_u64()).unwrap_or(1)
}

/// Envelope versions this binary folds (bd-asr2.9): v1 (legacy, no seq)
/// and v2 (subscriber scoping + budget rows). Anything else refuses
/// loudly — never fold-past into a mis-render.
pub fn journal_version_accepted(entry: &serde_json::Value) -> bool {
    journal_entry_version(entry) <= 2
}

/// All journal entries across every workspace, oldest first — active plus
/// retained generations (bd-asr2.4). Corrupt lines stop that workspace's
/// fold loudly (see [`journal_fold_status`]); this compat reader skips
/// past the tear so existing callers keep working, and strips the `.N`
/// generation suffix back to the workspace name.
pub fn journal_read_all(state_dir: &Path) -> Vec<(String, serde_json::Value)> {
    let store = doorbell_rt::FileStore::new(state_dir);
    let mut workspaces = std::collections::BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(state_dir.join("journals")) else {
        return Vec::new();
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name
            .strip_prefix("doorbell-journal-")
            .and_then(|n| n.strip_suffix(".jsonl"))
            && !rest.is_empty()
        {
            workspaces.insert(rest.split('.').next().unwrap_or(rest).to_string());
        }
    }
    let mut out = Vec::new();
    for ws in workspaces {
        let fold = store.fold_workspace(&ws);
        for row in fold.rows {
            out.push((ws.clone(), row));
        }
    }
    out
}

/// Loud fold status for `why` and `/health` (bd-asr2.4 D4.4): torn tail
/// (file, line, size, range, rows withheld) and the first foldable seq for
/// retention-honest queries. `None` torn + `None` first_seq = empty journal,
/// not an error.
///
/// bd-swad: the tear renders through `TornTail::describe`, so the SIZE travels
/// with the report instead of living only inside the reason sentence.
pub fn journal_fold_status(state_dir: &Path, workspace: &str) -> (Option<String>, Option<u64>) {
    let store = doorbell_rt::FileStore::new(state_dir);
    let fold = store.fold_workspace(workspace);
    let torn = fold.torn.map(|t| t.describe());
    (torn, fold.first_seq)
}

/// Workspaces with journal files (active only; generations fold with
/// their workspace). Used for zero-row survivors of fully torn folds.
pub fn journal_workspaces_list(state_dir: &Path) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    if let Ok(dir) = std::fs::read_dir(state_dir.join("journals")) {
        for entry in dir.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name
                .strip_prefix("doorbell-journal-")
                .and_then(|n| n.strip_suffix(".jsonl"))
                && !rest.is_empty()
                && !rest.contains('.')
            {
                out.insert(rest.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Scope-bound roster row (bd-asr2.14, D5/E2): who is around to be woken.
/// Pure: the digest commands assemble inputs (registry, binds, presence,
/// bead map) and render headers from rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    pub pin: String,
    pub harness: String,
    pub role: String,
    pub state: String,
    pub recency_secs: Option<u64>,
    pub bead: String,
}

/// Build scope-bound roster rows: registry pins joined with role binds,
/// presence states, and current-bead map. Unknowns stay blank (never
/// invent-green); recency is `now - ts` for fresh rows only.
pub fn build_roster(
    pins: &[(String, String)],
    roles: &std::collections::BTreeMap<String, String>,
    presence: &[(String, String, u64)],
    beads: &std::collections::BTreeMap<String, String>,
    now_unix: u64,
) -> Vec<RosterRow> {
    let mut rows = Vec::new();
    for (pin, harness) in pins {
        let (state, recency_secs) = presence
            .iter()
            .find(|(p, _, _)| p == pin)
            .map(|(_, s, ts)| (s.clone(), Some(now_unix.saturating_sub(*ts))))
            .unwrap_or_else(|| ("unknown".to_string(), None));
        rows.push(RosterRow {
            pin: pin.clone(),
            harness: harness.clone(),
            role: roles.get(pin).cloned().unwrap_or_default(),
            state,
            recency_secs,
            bead: beads.get(pin).cloned().unwrap_or_default(),
        });
    }
    rows
}

/// Render roster header lines for digest output (scope-bound, one row per
/// line; a digest for P carries Q's row — E2 discovery without introduction).
pub fn render_roster(rows: &[RosterRow]) -> Vec<String> {
    let mut out = vec!["ROSTER (scope-bound):".to_string()];
    for r in rows {
        let recency = r
            .recency_secs
            .map(|s| format!("{s}s ago"))
            .unwrap_or_else(|| "never".to_string());
        let role = if r.role.is_empty() {
            "-".to_string()
        } else {
            r.role.clone()
        };
        let bead = if r.bead.is_empty() {
            "-".to_string()
        } else {
            r.bead.clone()
        };
        out.push(format!(
            "  {} [{}] role={} state={} {} bead={}",
            r.pin, r.harness, role, r.state, recency, bead
        ));
    }
    out
}

/// Nearby discovery row (bd-gpj0.2): herdr panes ∪ registry pins ∪
/// reservation holders. Empty pin means an unbound pane — the thing four
/// manual queries used to hunt for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NearbyRow {
    pub pin: String,
    pub pane: String,
    pub name: String,
    pub state: String,
    pub held: String,
}

/// Pure join: registry `(pin, pane, name)`, herdr `(pane, name, status)`,
/// leases `(holder, path)`. Unbound panes stay in the list.
pub fn join_nearby(
    registry: &[(String, String, String)],
    herdr: &[(String, String, String)],
    leases: &[(String, String)],
) -> Vec<NearbyRow> {
    let mut herdr_by_pane: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    for (pane, name, status) in herdr {
        herdr_by_pane.insert(pane.clone(), (name.clone(), status.clone()));
    }
    let mut seen_panes = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    for (pin, pane, name) in registry {
        seen_panes.insert(pane.clone());
        let (hname, status) = herdr_by_pane
            .get(pane)
            .cloned()
            .unwrap_or_else(|| (name.clone(), "missing".to_string()));
        rows.push(NearbyRow {
            pin: pin.clone(),
            pane: pane.clone(),
            name: if hname.is_empty() {
                name.clone()
            } else {
                hname
            },
            state: status,
            held: lease_paths(leases, pin),
        });
    }
    for (pane, (name, status)) in &herdr_by_pane {
        if seen_panes.contains(pane) {
            continue;
        }
        rows.push(NearbyRow {
            pin: String::new(),
            pane: pane.clone(),
            name: name.clone(),
            state: status.clone(),
            held: lease_paths(leases, name),
        });
    }
    rows.sort_by(|a, b| a.pane.cmp(&b.pane).then(a.pin.cmp(&b.pin)));
    rows
}

fn lease_paths(leases: &[(String, String)], holder: &str) -> String {
    let mut paths: Vec<&str> = leases
        .iter()
        .filter(|(h, _)| h == holder)
        .map(|(_, p)| p.as_str())
        .collect();
    paths.sort_unstable();
    paths.join(",")
}

/// Human lines for `doorbell roster`. Empty pin/held render as `-`.
pub fn render_nearby(rows: &[NearbyRow]) -> Vec<String> {
    let mut out = vec!["NEARBY:".to_string()];
    for r in rows {
        let pin = if r.pin.is_empty() {
            "-"
        } else {
            r.pin.as_str()
        };
        let pane = if r.pane.is_empty() {
            "-"
        } else {
            r.pane.as_str()
        };
        let held = if r.held.is_empty() {
            "-"
        } else {
            r.held.as_str()
        };
        out.push(format!(
            "  pin={pin} pane={pane} name={} state={} held={held}",
            r.name, r.state
        ));
    }
    out
}

/// One introduce step (bd-gpj0.3): contact request, channel create, or
/// doorbell subscribe. Pure so the sequence is testable without I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntroduceStep {
    pub verb: &'static str,
    pub argv: Vec<String>,
}

/// Default channel id for introducing `pin`.
pub fn introduce_channel_id(pin: &str) -> String {
    format!("intro-{}", pin.to_ascii_lowercase())
}

/// Ordered introduce plan: contact request → channel create (creator
/// joins) → subscribe the peer pin. Caller supplies resolved slugs.
pub fn introduce_steps(
    project: &str,
    pin: &str,
    as_pin: &str,
    channel: &str,
    workspace: &str,
) -> Vec<IntroduceStep> {
    vec![
        IntroduceStep {
            verb: "contact",
            argv: vec![
                "contact".into(),
                "request".into(),
                "--to".into(),
                pin.into(),
                "--as".into(),
                as_pin.into(),
                "--project".into(),
                project.into(),
                "--reason".into(),
                "introduce".into(),
            ],
        },
        IntroduceStep {
            verb: "channel",
            argv: vec![
                "channel".into(),
                "create".into(),
                channel.into(),
                "--name".into(),
                format!("intro {pin}"),
                "--project".into(),
                project.into(),
                "--as".into(),
                as_pin.into(),
            ],
        },
        IntroduceStep {
            verb: "subscribe",
            argv: vec![
                "--workspace".into(),
                workspace.into(),
                "--pin".into(),
                pin.into(),
                "--project".into(),
                project.into(),
            ],
        },
    ]
}

/// True only for a prior successful introduce, never a missing resource.
/// `"does not exist"` must not count (ce-review finding on bd-gpj0.3).
pub fn introduce_idempotent(msg: &str) -> bool {
    let s = msg.to_ascii_lowercase();
    s.contains("already") || s.contains("idempotent")
}

/// Cross-workspace scopes (bd-asr2.18, D5, flag-gated): widen one pin's
/// Scope stays explicit (Scope::Many, deduped) — never an implicit
/// all-workspace watch. Flag off or non-operator actor refuses.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub enum CrossScopeDeny {
    FlagOff,
    NoSubscription,
    NeedsOperator,
    Store(doorbell_rt::subscribe::Refusal),
}

// A test-only external-unblock sweep used to live here, with its own
// `br`-shaped command seam and `parse_external_corr` copy. It is deleted: the
// live path is `doorbell_rt::pollers::spawn_external_rering` (called from
// `src/doorbell_host.rs`), whose `detect_external_closed` covers the same corr
// parse plus fire-once, and which refuses ids containing a quote. The copy
// also resolved the hold itself — a job the live poller deliberately leaves to
// the waiter — so keeping it risked two answers to one question.

#[cfg(test)]
pub fn journal_append(
    state_dir: &Path,
    workspace: &str,
    msg: &crate::doorbell_tick::DoorbellMsg,
    actions: &[crate::doorbell_tick::DoorbellAction],
    executed: serde_json::Value,
) {
    let path = journal_file(state_dir, workspace);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entry = serde_json::json!({
        "v": 1,
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "workspace": workspace,
        "msg": msg,
        "actions": actions,
        "executed": executed,
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{entry}");
    }
    // F8: no second retention impl here — rotation plus audit lives in
    // doorbell-rt FileStore (RETENTION_LINES/BYTES). Legacy silent head-trim
    // deleted; live path uses journal_append_workspace via the store.
}

/// One journaled tick: the Msg in, the Actions out, what the effect layer
/// actually did. `doorbell why <pin>` replays this — never log tailing.
#[cfg(test)]
pub fn journal_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("journals")
        .join(format!("doorbell-journal-{workspace}.jsonl"))
}

#[cfg(test)]
pub fn subscribe_extra_workspace_in(
    state_dir: &Path,
    workspace: &str,
    pin: &str,
    extra: &str,
    operator: bool,
    cross_on: bool,
) -> Result<Vec<String>, CrossScopeDeny> {
    use doorbell_rt::subscribe::{Actor, Scope, SubscriptionStore};
    if !cross_on {
        return Err(CrossScopeDeny::FlagOff);
    }
    if !operator {
        return Err(CrossScopeDeny::NeedsOperator);
    }
    let mut store = SubscriptionStore::open(state_dir);
    let id = format!("spawn-{workspace}-{pin}");
    let mut sub = store.get(&id).ok_or(CrossScopeDeny::NoSubscription)?;
    let mut ws = sub.scope.workspaces();
    if !ws.contains(&extra.to_string()) {
        ws.push(extra.to_string());
    }
    sub.scope = Scope::Many(ws.clone());
    store
        .subscribe(Actor::operator(), sub, workspace)
        .map_err(CrossScopeDeny::Store)?;
    Ok(ws)
}

/// Drop injects that exceed the pin's 5/30/3 budget (rolling).
#[cfg(test)]
pub fn take_allowed(
    injects: Vec<(String, String)>,
    budgets: &mut BTreeMap<String, PinBudget>,
    now_unix: u64,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (pin, msg) in injects {
        let b = budgets
            .entry(pin.clone())
            .or_insert_with(|| PinBudget::adr_default(now_unix));
        if b.allows(now_unix) {
            b.record_at(now_unix);
            out.push((pin, msg));
        }
    }
    out
}

#[cfg(test)]
pub fn write_unanswered(path: &Path, pins: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let body = if pins.is_empty() {
        String::new()
    } else {
        let mut pins = pins.to_vec();
        pins.sort();
        format!("{}\n", pins.join("\n"))
    };
    fs::write(path, body).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RingInput {
        RingInput {
            kind: 9,
            addressed_to_self: false,
            toron_kind_request_release: false,
            toron_kind_mail_delivered: false,
            toron_kind_ack_sla: false,
            importance_high_or_urgent: false,
            reservation_conflicts_self: false,
            is_presence: false,
            is_ack_only: false,
        }
    }

    #[test]
    fn sc_d1_kind9_at_self_rings_channel() {
        let mut ev = base();
        ev.addressed_to_self = true;
        assert_eq!(classify(&ev), Some(EventClass::Channel));
    }

    #[test]
    fn room_chatter_without_mention_does_not_ring() {
        assert_eq!(classify(&base()), None);
    }

    #[test]
    fn presence_never_rings() {
        let mut ev = base();
        ev.kind = 20001;
        ev.is_presence = true;
        ev.addressed_to_self = true;
        assert_eq!(classify(&ev), None);
    }

    #[test]
    fn sc_d2_request_release_rings_holder() {
        let mut ev = base();
        ev.kind = 14;
        ev.toron_kind_request_release = true;
        ev.addressed_to_self = true;
        assert_eq!(classify(&ev), Some(EventClass::RequestRelease));
    }

    #[test]
    fn job_for_self_rings() {
        let mut ev = base();
        ev.kind = 43001;
        ev.addressed_to_self = true;
        assert_eq!(classify(&ev), Some(EventClass::Job));
    }

    #[test]
    fn reservation_conflict_rings() {
        let mut ev = base();
        ev.kind = 30031;
        ev.reservation_conflicts_self = true;
        assert_eq!(classify(&ev), Some(EventClass::ReservationConflict));
    }

    #[test]
    fn sc_d3_queue_coalesces_per_class() {
        let mut q = DoorbellQueue::default();
        // Same hint coalesces (per-channel key, same Pull)
        q.push(EventClass::Channel, doorbell_line(EventClass::Channel, "a"));
        q.push(EventClass::Channel, doorbell_line(EventClass::Channel, "a"));
        q.push(EventClass::Job, doorbell_line(EventClass::Job, "j"));
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert!(q.is_empty());
        assert!(
            drained[0].1.contains("[doorbell]"),
            "line is a doorbell, got {}",
            drained[0].1
        );
        // Different channels keep both lines (gap 6 fix: per-channel)
        let mut q2 = DoorbellQueue::default();
        q2.push(
            EventClass::Channel,
            doorbell_line(EventClass::Channel, "h=alpha"),
        );
        q2.push(
            EventClass::Channel,
            doorbell_line(EventClass::Channel, "h=beta"),
        );
        assert_eq!(q2.len(), 2, "different hints must not coalesce");
    }

    #[test]
    fn sc_d3_inject_only_when_idle_or_done() {
        assert!(!should_inject("working"));
        assert!(should_inject("idle"));
        assert!(should_inject("done"));
        assert!(should_inject("blocked"));
        assert!(is_drain_transition("working", "idle"));
        assert!(is_drain_transition("working", "done"));
        assert!(is_drain_transition("working", "blocked"));
        assert!(!is_drain_transition("idle", "working"));
        assert!(!is_drain_transition("blocked", "working"));
    }

    #[test]
    fn pid_stale_lock_reloads() {
        let dir = std::env::temp_dir().join(format!("fw-doorbell-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = pid_file(&dir, "w81");
        write_pid(&path, 999_999).expect("write");
        assert_eq!(
            inspect_lock_with(&path, |_| false),
            LockAction::Reloaded { stale: 999_999 }
        );
        assert_eq!(
            inspect_lock_with(&path, |_| true),
            LockAction::Attach { pid: 999_999 }
        );
        clear_pid(&path);
        assert_eq!(inspect_lock_with(&path, |_| true), LockAction::Start);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fanout_skips_working_pins() {
        let rings = vec![
            ("BlueRidge".into(), EventClass::Channel, "ch".into()),
            ("LilacChapel".into(), EventClass::Job, "job".into()),
        ];
        let mut status = BTreeMap::new();
        status.insert("BlueRidge".into(), "working".into());
        status.insert("LilacChapel".into(), "idle".into());
        let out = fanout_now(&rings, &status);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "LilacChapel");
        assert!(out[0].1.contains("[doorbell]"));
    }

    #[test]
    fn parse_watch_kind9_mention() {
        let line =
            r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","steer-x"]],"content":"hi"}"#;
        let (class, _) =
            parse_watch_ndjson(line, &SelfIndex::from_pins(["BlueRidge"])).expect("ring");
        assert_eq!(class, EventClass::Channel);
        assert!(parse_watch_ndjson(line, &SelfIndex::from_pins(["LilacChapel"])).is_none());
    }

    fn kind9_at_blueridge() -> &'static str {
        r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","steer-x"]],"content":"@BlueRidge hi"}"#
    }

    #[test]
    fn sc_d1_kind9_at_pin_is_routine_without_prompt_when_idle() {
        // bd-l72k supersedes the old inject: addressed kind-9 still routes
        // at the edge (doorbell::classify unchanged) but workspace triage
        // demotes it to routine — zero prompts, counters only.
        // Ported to workspace::tick per bd-3wzl — no second reducer
        let mut model = crate::workspace::WorkspaceModel::default();
        model
            .doorbell
            .statuses
            .insert("BlueRidge".into(), "idle".into());
        model
            .doorbell
            .statuses
            .insert("LilacChapel".into(), "idle".into());
        model.doorbell.index = SelfIndex::from_pins(["BlueRidge", "LilacChapel"]);
        // D3 (bd-s8bo): herdr idle is evidence, never authority — the exact
        // Idle verdict below is what lets the Prompt through the gate.
        model.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        model.presence.insert(
            "LilacChapel".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        let (next, cmds) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson {
                line: kind9_at_blueridge().to_string(),
            },
        );
        let prompts: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                crate::workspace::Cmd::Prompt { pin, line } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        assert!(
            prompts.is_empty(),
            "bd-l72k: routine kind-9 spends zero prompts, got {prompts:?}"
        );
        assert!(next.doorbell.queues.is_empty(), "idle pin must not queue");
        assert_eq!(
            next.wake_counters.get("routine"),
            Some(&1),
            "routine tick counted: {:?}",
            next.wake_counters
        );
        assert!(
            next.wake.digests.is_empty(),
            "routine never enters the digest"
        );
    }

    #[test]
    fn sc_d1_kind9_skips_unmentioned_pin() {
        let mut status = BTreeMap::new();
        status.insert("BlueRidge".into(), "idle".into());
        status.insert("LilacChapel".into(), "idle".into());
        // D3 (bd-s8bo): the once/debug path carries no presence authority,
        // so herdr-only idle stays silent for every pin — mentioned or not.
        // Mention routing with authority is covered at tick level by
        // sc_d1_kind9_at_pin_injects_when_idle.
        let json = once_json(kind9_at_blueridge(), &status);
        assert_eq!(
            json, "[]",
            "herdr-only idle must not inject without presence authority: {json}"
        );
    }

    #[test]
    fn once_json_seeds_statuses_via_herdr_tick() {
        // bd-adr0027-proven-leftovers-55nx.10 RED: statuses must enter via
        // Msg::HerdrStatus ticks, never via direct map assign. (Needle is
        // split so this source-scan does not match its own literal.)
        let src = include_str!("doorbell.rs");
        let needle = ["model.doorbell.statuses ", "= status_by_pin"].concat();
        assert!(
            !src.contains(&needle),
            "RED: once_json still map-assigns statuses; tick Msg::HerdrStatus instead"
        );
        // Behavioral: working pin must queue (no prompt). D3 (bd-s8bo):
        // herdr idle alone never prompts (never trusted) — the once/debug
        // path carries no presence authority, so idle stays silent too.
        let mut working = BTreeMap::new();
        working.insert("BlueRidge".into(), "working".into());
        let json = once_json(kind9_at_blueridge(), &working);
        assert_eq!(json, "[]", "working pin must not inject: {json}");
        let mut idle = BTreeMap::new();
        idle.insert("BlueRidge".into(), "idle".into());
        let json = once_json(kind9_at_blueridge(), &idle);
        assert_eq!(
            json, "[]",
            "herdr-only idle must not inject without presence authority: {json}"
        );
    }

    #[test]
    fn s_proof_why_producer_journal_path() {
        // zfnx.9 producer contract: journal path is
        // state_dir/journals/doorbell-journal-{workspace}.jsonl.
        let dir = std::env::temp_dir().join(format!("fw-sproof-path-{}", std::process::id()));
        let path = journal_file(&dir, "ws1");
        assert_eq!(
            path,
            dir.join("journals").join("doorbell-journal-ws1.jsonl"),
            "journal path contract violated: {path:?}"
        );
    }

    #[test]
    fn s_proof_why_fold_reproduces_cmds() {
        // zfnx.9: fold tick from Default over journal entries reproduces the
        // recorded Vec<Cmd> — the why trust surface. Uses an index-free
        // Receipt msg so the fold is exact (see gap note below).
        let dir = std::env::temp_dir().join(format!("fw-sproof-cmds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let msg = crate::workspace::Msg::Receipt {
            corr: "FM-SPROOF".into(),
            receipt: crate::workspace::Receipt {
                state: "owed".into(),
                wake: "tasked".into(),
                due_at: None,
                result_path: None,
            },
        };
        let (_, cmds) = crate::workspace::tick(&crate::workspace::WorkspaceModel::default(), &msg);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                crate::workspace::Cmd::SqliteUpsert { corr, .. } if corr == "FM-SPROOF"
            )),
            "seed must produce SqliteUpsert, got {cmds:?}"
        );
        journal_append_workspace(&dir, "ws1", &msg, &cmds, serde_json::json!({}), Some(&[]));
        let entries = journal_read_all(&dir);
        assert_eq!(entries.len(), 1, "roundtrip must read 1 entry: {entries:?}");
        let mut folded = crate::workspace::WorkspaceModel::default();
        for (workspace, entry) in &entries {
            assert_eq!(workspace, "ws1");
            if let Some(ps) = entry.get("pins").and_then(|p| p.as_array()) {
                for pin in ps.iter().filter_map(|v| v.as_str()) {
                    folded.doorbell.index.insert_pin(pin.to_string(), None);
                }
            }
            let m: crate::workspace::Msg =
                serde_json::from_value(entry.get("msg").cloned().unwrap()).unwrap();
            let expected: Vec<crate::workspace::Cmd> =
                serde_json::from_value(entry.get("actions").cloned().unwrap()).unwrap();
            let (n, actual) = crate::workspace::tick(&folded, &m);
            folded = n;
            assert_eq!(
                actual, expected,
                "fold must reproduce recorded Cmds for {m:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn s_proof_why_fold_reproduces_prompt_with_pins() {
        // zfnx.9 + bd-z54w: exact Vec<Cmd> reproduction INCLUDING a Prompt —
        // the journaled pin set restores the fold index deterministically.
        let dir = std::env::temp_dir().join(format!("fw-sproof-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pins = vec!["BlueRidge".to_string()];
        let mut model = crate::workspace::WorkspaceModel::default();
        for p in &pins {
            model.doorbell.index.insert_pin(p.clone(), None);
        }
        let idle = crate::workspace::Msg::HerdrStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        };
        let (seeded, _) = crate::workspace::tick(&model, &idle);
        model = seeded;
        // D3 (bd-s8bo): the Prompt needs presence authority; journal the
        // Presence msg so the replay fold reproduces it exactly.
        let presence = crate::workspace::Msg::Presence {
            pin: "BlueRidge".into(),
            state: "idle".into(),
        };
        let (seeded, _) = crate::workspace::tick(&model, &presence);
        model = seeded;
        let msg = crate::workspace::Msg::WatchNdjson {
            // bd-l72k: only urgent lines deliver prompts — seed the
            // round-trip with an urgent wake so the fold still proves
            // Prompt reproduction (routine wakes are silent by design).
            line: r#"{"kind":14,"id":"u1","tags":[["p","BlueRidge"],["toron-kind","request-release"]],"content":"release?"}"#.to_string(),
        };
        let (next, cmds) = crate::workspace::tick(&model, &msg);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                crate::workspace::Cmd::Prompt { pin, .. } if pin == "BlueRidge"
            )),
            "seed must produce a Prompt, got {cmds:?}"
        );
        let _ = next;
        journal_append_workspace(
            &dir,
            "ws1",
            &presence,
            &[],
            serde_json::json!({}),
            Some(&pins),
        );
        journal_append_workspace(&dir, "ws1", &idle, &[], serde_json::json!({}), Some(&pins));
        journal_append_workspace(&dir, "ws1", &msg, &cmds, serde_json::json!({}), Some(&pins));
        let entries = journal_read_all(&dir);
        assert_eq!(entries.len(), 3, "roundtrip must read 3 entries");
        let mut folded = crate::workspace::WorkspaceModel::default();
        for (workspace, entry) in &entries {
            assert_eq!(workspace, "ws1");
            // Same seeding rule as cmd_doorbell_why: pins restore the index,
            // missing pins keep Unknown semantics.
            if let Some(ps) = entry.get("pins").and_then(|p| p.as_array()) {
                for pin in ps.iter().filter_map(|v| v.as_str()) {
                    folded.doorbell.index.insert_pin(pin.to_string(), None);
                }
            }
            let m: crate::workspace::Msg =
                serde_json::from_value(entry.get("msg").cloned().unwrap()).unwrap();
            let expected: Vec<crate::workspace::Cmd> =
                serde_json::from_value(entry.get("actions").cloned().unwrap()).unwrap();
            let (n, actual) = crate::workspace::tick(&folded, &m);
            folded = n;
            assert_eq!(
                actual, expected,
                "fold must reproduce recorded Cmds exactly, including Prompt"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edge_triage_journal_roundtrip_full_cycle() {
        // bd-l72k E2E (pure level): fixture journal (benign + routine +
        // captain + urgent) through file round-trip, refolded from Default
        // with pins restoring the index — markers advance, the digest
        // drains the captain set, only the urgent row injects.
        use crate::workspace::{Cmd, Msg, WorkspaceModel};
        let dir = std::env::temp_dir().join(format!("fw-edge-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pins = vec!["BlueRidge".to_string()];
        let mut model = WorkspaceModel::default();
        for p in &pins {
            model.doorbell.index.insert_pin(p.clone(), None);
        }
        let presence = Msg::Presence {
            pin: "BlueRidge".into(),
            state: "idle".into(),
        };
        let (next, cmds) = crate::workspace::tick(&model, &presence);
        assert!(cmds.is_empty());
        journal_append_workspace(
            &dir,
            "ws1",
            &presence,
            &cmds,
            serde_json::json!({}),
            Some(&pins),
        );
        model = next;
        let idle = Msg::HerdrStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        };
        let (next, cmds) = crate::workspace::tick(&model, &idle);
        journal_append_workspace(
            &dir,
            "ws1",
            &idle,
            &cmds,
            serde_json::json!({}),
            Some(&pins),
        );
        model = next;
        let rows = [
            r#"{"kind":9,"id":"b1","tags":[["h","ch-1"]],"content":"room talk"}"#,
            r#"{"kind":9,"id":"r1","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#,
            r#"{"kind":43001,"id":"c1","tags":[["p","BlueRidge"],["h","steer-x"]],"content":"job ready"}"#,
            r#"{"kind":14,"id":"u1","tags":[["p","BlueRidge"],["toron-kind","request-release"]],"content":"release?"}"#,
        ];
        for line in rows {
            let msg = Msg::WatchNdjson {
                line: line.to_string(),
            };
            let (next, cmds) = crate::workspace::tick(&model, &msg);
            journal_append_workspace(&dir, "ws1", &msg, &cmds, serde_json::json!({}), Some(&pins));
            model = next;
        }
        let prompts: Vec<_> = model
            .wake
            .digests
            .iter()
            .filter(|e| e.kind == "urgent-prompt")
            .collect();
        assert_eq!(
            prompts.len(),
            1,
            "urgent row recorded once: {:?}",
            model.wake.digests
        );
        // Refold from Default: every recorded Cmd sequence reproduces.
        let entries = journal_read_all(&dir);
        assert_eq!(entries.len(), 6, "presence+idle+4 rows round-trip");
        let mut folded = WorkspaceModel::default();
        let mut live_prompts = 0;
        for (workspace, entry) in &entries {
            assert_eq!(workspace, "ws1");
            if let Some(ps) = entry.get("pins").and_then(|p| p.as_array()) {
                for pin in ps.iter().filter_map(|v| v.as_str()) {
                    folded.doorbell.index.insert_pin(pin.to_string(), None);
                }
            }
            let m: Msg = serde_json::from_value(entry.get("msg").cloned().unwrap()).unwrap();
            let expected: Vec<Cmd> =
                serde_json::from_value(entry.get("actions").cloned().unwrap()).unwrap();
            let (n, actual) = crate::workspace::tick(&folded, &m);
            folded = n;
            assert_eq!(actual, expected, "fold reproduces recorded Cmds for {m:?}");
            live_prompts += actual
                .iter()
                .filter(|c| matches!(c, Cmd::Prompt { .. }))
                .count();
        }
        assert_eq!(live_prompts, 1, "only the urgent row injects on refold");
        assert_eq!(
            folded.wake.digests.len(),
            2,
            "captain entry plus urgent record: {:?}",
            folded.wake.digests
        );
        assert!(
            folded.wake_counters.get("routine").copied().unwrap_or(0) >= 1
                && folded.wake_counters.get("benign").copied().unwrap_or(0) >= 1,
            "markers advance on refold: {:?}",
            folded.wake_counters
        );
        // Digest drains on a now-bearing beat.
        let (drained, cmds) = crate::workspace::tick(&folded, &Msg::Tick { now: 1_000_000 });
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::Digest { .. })),
            "digest batch emits: {cmds:?}"
        );
        assert!(drained.wake.digests.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "bd-l72k LIVE probe: needs live toron daemon + flywheel project + TurquoiseSparrow pin"]
    fn edge_triage_live_probe_urgent_only_injects() {
        // bd-l72k AC(2): a live pin receiving benign room noise plus one
        // urgent mail shows only the urgent injection through
        // workspace::tick. Run explicitly; never in CI.
        use std::process::Command;
        let proj = "flywheel";
        let pin = "TurquoiseSparrow";
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let marker = format!("edge-probe-{}-{}", std::process::id(), now);
        let run = |args: &[&str]| -> String {
            let out = Command::new("toron")
                .args(args)
                .output()
                .expect("toron CLI on PATH for live probe");
            assert!(
                out.status.success(),
                "live probe needs the toron CLI to succeed: {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        // Benign room noise on a scratch channel (create/join idempotent).
        run(&[
            "channel",
            "create",
            "edge-triage-probe",
            "--name",
            "edge-triage-probe",
            "--project",
            proj,
            "--as",
            pin,
        ]);
        run(&[
            "channel",
            "join",
            "edge-triage-probe",
            "--project",
            proj,
            "--as",
            pin,
        ]);
        run(&[
            "channel",
            "post",
            "edge-triage-probe",
            "--project",
            proj,
            "--as",
            pin,
            "--body",
            &format!("benign noise {marker}"),
        ]);
        // One urgent plane-P mail to self.
        run(&[
            "mail",
            "send",
            "--project",
            proj,
            "--as",
            pin,
            "--to",
            pin,
            "--subject",
            &format!("urgent {marker}"),
            "--body",
            "release please",
            "--importance",
            "urgent",
        ]);
        // Capture the benign line live off the bus.
        let mut benign_line = None;
        for _ in 0..3 {
            let out = run(&[
                "watch",
                "--project",
                proj,
                "--kinds",
                "9",
                "--channel",
                "edge-triage-probe",
                "--once",
                "--timeout-ms",
                "8000",
            ]);
            benign_line = out.lines().find_map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).ok()?;
                if v.get("content")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(&marker))
                {
                    Some(l.to_string())
                } else {
                    None
                }
            });
            if benign_line.is_some() {
                break;
            }
        }
        let benign_line = benign_line.expect("live probe must observe its benign post");
        // Pull the urgent row live from the inbox.
        let mut urgent_row: Option<serde_json::Value> = None;
        for _ in 0..5 {
            let out = Command::new("flywheel")
                .args([
                    "mail", "inbox", proj, "--as", pin, "--since", "last", "--json",
                ])
                .output()
                .expect("flywheel CLI on PATH for live probe");
            if let Ok(v) =
                serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&out.stdout))
            {
                let items = if let Some(a) = v.as_array() {
                    a.clone()
                } else {
                    vec![v]
                };
                urgent_row = items.into_iter().find(|r| {
                    r.get("subject")
                        .and_then(|s| s.as_str())
                        .is_some_and(|s| s.contains(&marker))
                });
            }
            if urgent_row.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        let urgent_row = urgent_row.expect("live probe must observe its urgent mail");
        // Both through the triage tick with presence authority.
        let mut model = crate::workspace::WorkspaceModel::default();
        model.doorbell.index = SelfIndex::from_pins([pin.to_string()]);
        model
            .doorbell
            .statuses
            .insert(pin.to_string(), "idle".into());
        model.presence.insert(
            pin.to_string(),
            crate::workspace::Presence::from_str("idle"),
        );
        let (m1, cmds1) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson { line: benign_line },
        );
        assert!(
            cmds1
                .iter()
                .all(|c| !matches!(c, crate::workspace::Cmd::Prompt { .. })),
            "live benign noise spends zero prompts: {cmds1:?}"
        );
        let (m2, cmds2) = crate::workspace::tick(
            &m1,
            &crate::workspace::Msg::InboxRows {
                rows: vec![urgent_row],
                pin: pin.to_string(),
            },
        );
        let urgent_prompts: Vec<_> = cmds2
            .iter()
            .filter_map(|c| match c {
                crate::workspace::Cmd::Prompt { pin, line } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            urgent_prompts.len(),
            1,
            "live urgent mail injects exactly once, got {urgent_prompts:?}"
        );
        let _ = m2;
    }

    #[test]
    fn s_proof_why_fold_without_pins_never_invents_idle() {
        // Entries lacking pins (pre-field journals) fold with Unknown-presence
        // semantics: a kind-9 mention for an unknown pin stays silent.
        let mut folded = crate::workspace::WorkspaceModel::default();
        let m = crate::workspace::Msg::WatchNdjson {
            line: kind9_at_blueridge().to_string(),
        };
        let (_, cmds) = crate::workspace::tick(&folded, &m);
        assert!(
            cmds.iter()
                .all(|c| !matches!(c, crate::workspace::Cmd::Prompt { .. })),
            "pins-less fold must never invent a Prompt, got {cmds:?}"
        );
        let _ = &mut folded;
    }

    #[test]
    fn sc_d3_working_queues_then_drains_on_idle() {
        // Ported to workspace::tick per bd-3wzl
        let mut model = crate::workspace::WorkspaceModel::default();
        model
            .doorbell
            .statuses
            .insert("BlueRidge".into(), "working".into());
        model.doorbell.index = SelfIndex::from_pins(["BlueRidge"]);
        let (next, cmds) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::WatchNdjson {
                line: kind9_at_blueridge().to_string(),
            },
        );
        let prompts: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                crate::workspace::Cmd::Prompt { pin, line } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        assert!(prompts.is_empty(), "working pin must not inject");
        assert_eq!(next.doorbell.queues.get("BlueRidge").map(Vec::len), Some(1));
        // D3 (bd-s8bo): the drain needs presence authority; herdr idle
        // alone stays Unknown and silent.
        let mut next = next;
        next.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        let (next2, cmds2) = crate::workspace::tick(
            &next,
            &crate::workspace::Msg::HerdrStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let drained: Vec<_> = cmds2
            .iter()
            .filter_map(|c| match c {
                crate::workspace::Cmd::Prompt { pin, line } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        assert!(
            drained.is_empty(),
            "bd-l72k: drained routine wakes spend zero prompts, got {drained:?}"
        );
        assert!(
            next2
                .doorbell
                .queues
                .get("BlueRidge")
                .is_none_or(Vec::is_empty),
            "drain still empties the doorbell queue"
        );
    }

    #[test]
    fn sc_d1_kind9_npub_p_tag_rings_pin() {
        // bd-l72k: plain kind-9 no longer delivers, so npub resolution is
        // proven through an urgent (ack-sla-expired) line — the resolution
        // mechanism (SelfIndex npub lookup) is identical. (bd-v8vi:
        // mail-delivered now rings routine, so it can no longer serve as
        // the urgent vehicle here.)
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let line = format!(
            r#"{{"kind":9,"id":"ab","tags":[["p","{npub}"],["toron-kind","ack-sla-expired"],["h","steer-x"]],"content":"sealed"}}"#
        );
        let mut index = SelfIndex::default();
        index.insert_pin("BlueRidge".into(), Some(npub.into()));
        index.insert_pin("LilacChapel".into(), None);
        let mut model = crate::workspace::WorkspaceModel::default();
        model
            .doorbell
            .statuses
            .insert("BlueRidge".into(), "idle".into());
        model
            .doorbell
            .statuses
            .insert("LilacChapel".into(), "idle".into());
        model.doorbell.index = index;
        // D3 (bd-s8bo): exact Idle verdict required; herdr idle alone is evidence.
        model.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        let (_, cmds) =
            crate::workspace::tick(&model, &crate::workspace::Msg::WatchNdjson { line });
        let prompts: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                crate::workspace::Cmd::Prompt { pin, line } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            prompts.len(),
            1,
            "npub p-tag must resolve to pin, got {prompts:?}"
        );
        assert_eq!(prompts[0].0, "BlueRidge");
        assert!(prompts[0].1.contains("[doorbell]"));
    }

    #[test]
    fn sc_d1_kind9_at_pin_in_content_rings_without_pin_p_tag() {
        // bd-l72k: content-@mention routing is proven at the routing seam
        // (watch_targets) — plain kind-9 ticks no longer deliver prompts.
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let line = format!(
            r#"{{"kind":9,"id":"ab","tags":[["p","{npub}"],["h","steer-x"]],"content":"@BlueRidge take the repro"}}"#
        );
        let mut model = crate::workspace::WorkspaceModel::default();
        model
            .doorbell
            .statuses
            .insert("BlueRidge".into(), "idle".into());
        model.doorbell.index = SelfIndex::from_pins(["BlueRidge"]);
        // D3 (bd-s8bo): exact Idle verdict required; herdr idle alone is evidence.
        model.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        // also insert npub mapping so p-tag resolves but content also rings
        model
            .doorbell
            .index
            .insert_pin("BlueRidge".into(), Some(npub.into()));
        let targets = watch_targets(&line, &model.doorbell.index);
        assert_eq!(
            targets,
            vec!["BlueRidge".to_string()],
            "content @pin must route even when p is npub, got {targets:?}"
        );
    }

    #[test]
    fn sc_d2_inbox_request_release_rings() {
        let raw = r#"{"importance":"high","tags":[["toron-kind","request-release"]],"created_ts":"2026-08-20T00:00:01Z"}"#;
        let (class, hint) = parse_inbox_item(&serde_json::from_str(raw).unwrap()).expect("ring");
        assert_eq!(class, EventClass::RequestRelease);
        assert!(hint.contains("inbox --since"));
    }

    #[test]
    fn sc_d2_routine_inbox_does_not_ring() {
        let raw = r#"{"importance":"normal","ack_only":false}"#;
        assert!(parse_inbox_item(&serde_json::from_str(raw).unwrap()).is_none());
    }

    #[test]
    fn plane_p_urgent_injects_when_idle() {
        // bd-adr0027-proven-leftovers-55nx.2: ported from the deleted
        // apply_inbox_json second reducer to the live path —
        // workspace::tick + Msg::InboxRows. Statuses enter as HerdrStatus
        // Msgs, never a side map.
        use crate::workspace::{Msg, WorkspaceModel};
        let mut model = WorkspaceModel::default();
        model.doorbell.index.insert_pin("BlueRidge".into(), None);
        // D3 (bd-s8bo): urgent still needs an exact Idle verdict; herdr
        // idle alone stays Unknown and silent.
        model.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        let (model, _) = crate::workspace::tick(
            &model,
            &Msg::HerdrStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let row = serde_json::json!({
            "importance": "urgent",
            "created_ts": "2026-08-20T00:00:02Z",
        });
        let (_, cmds) = crate::workspace::tick(
            &model,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "BlueRidge".into(),
            },
        );
        let prompts: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                crate::workspace::Cmd::Prompt { pin, line } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].1.contains("[doorbell]"));
        assert!(prompts[0].1.contains("inbox --since"));
        let raw = r#"{"messages":[{"importance":"urgent","created_ts":"2026-08-20T00:00:02Z"}]}"#;
        assert_eq!(
            inbox_high_water(raw).as_deref(),
            Some("2026-08-20T00:00:02Z")
        );
    }

    #[test]
    fn inbox_high_water_reads_created_ts_us_from_live_json() {
        // Live ToJson emits created_ts_us (i64 micros), not created_ts RFC3339.
        let us = chrono::DateTime::parse_from_rfc3339("2026-08-20T00:00:02Z")
            .unwrap()
            .timestamp_micros();
        let raw = format!(
            r#"{{"messages":[{{"importance":"urgent","created_ts_us":{us},"body_md":"hi"}}]}}"#
        );
        let hw = inbox_high_water(&raw).expect("high-water from created_ts_us");
        let got = parse_rfc3339_micros(&hw).expect("hw is RFC3339");
        assert_eq!(got, us, "cursor must follow rumor time, not wall-clock now");
    }

    #[test]
    fn budget_stops_after_max_prompts() {
        let mut budgets = BTreeMap::new();
        budgets.insert("BlueRidge".into(), PinBudget::new(30, 1, 0));
        let injects = vec![
            ("BlueRidge".into(), "[doorbell] a".into()),
            ("BlueRidge".into(), "[doorbell] b".into()),
        ];
        let out = take_allowed(injects, &mut budgets, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "[doorbell] a");
    }

    #[test]
    fn budget_rolls_after_window() {
        let mut budgets = BTreeMap::new();
        budgets.insert("BlueRidge".into(), PinBudget::new(1, 1, 0));
        // First prompt at t=0 consumes the single token
        let out = take_allowed(
            vec![("BlueRidge".into(), "[doorbell] a".into())],
            &mut budgets,
            0,
        );
        assert_eq!(out.len(), 1);
        // Second prompt at t=10 still within window → blocked
        let out = take_allowed(
            vec![("BlueRidge".into(), "[doorbell] b".into())],
            &mut budgets,
            10,
        );
        assert!(out.is_empty(), "still within 1m window");
        // After 70s (1m+ window) the bucket refills → allowed again
        let out = take_allowed(
            vec![("BlueRidge".into(), "[doorbell] c".into())],
            &mut budgets,
            70,
        );
        assert_eq!(out.len(), 1, "rolling window should refill after 1m");
    }

    #[test]
    fn identity_prime_is_one_ascii_line() {
        let line = identity_prime("flywheel-toron-coder", "BlueRidge");
        assert!(line.contains("You are flywheel-toron-coder"));
        assert!(line.contains("Your mail identity is BlueRidge"));
        assert!(line.contains("Guard will block"));
        assert!(line.is_ascii());
        assert!(!line.chars().any(char::is_control));
    }

    #[test]
    fn sc_d5_status_line_up_and_down() {
        let dir = std::env::temp_dir().join(format!("fw-doorbell-st-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("doorbell"));
        assert_eq!(dispatcher_status_line(&dir, "w1"), "dispatcher: down");
        write_pid(&pid_file(&dir, "w1"), std::process::id()).expect("pid");
        let line = dispatcher_status_line(&dir, "w1");
        assert!(line.starts_with("dispatcher: up"), "{line}");
        clear_pid(&pid_file(&dir, "w1"));
        assert_eq!(dispatcher_status_line(&dir, "w1"), "dispatcher: down");
        write_pid(&pid_file(&dir, "w1"), 999_998).expect("stale");
        stop(&dir, "w1");
        assert!(!pid_file(&dir, "w1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatcher_token_absent_down_up() {
        let dir = std::env::temp_dir().join(format!("fw-doorbell-token-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("doorbell"));
        assert_eq!(dispatcher_token(&dir, "w1"), "absent");
        write_pid(&pid_file(&dir, "w1"), 999_998).expect("stale");
        assert_eq!(dispatcher_token(&dir, "w1"), "down");
        write_pid(&pid_file(&dir, "w1"), std::process::id()).expect("live");
        assert_eq!(dispatcher_token(&dir, "w1"), "up");
        // Don't stop() while the pid file names this test process.
        clear_pid(&pid_file(&dir, "w1"));
        assert_eq!(dispatcher_token(&dir, "w1"), "absent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_workspaces_includes_swarm_label_when_registry_empty() {
        let keys = candidate_workspaces("flywheel-toron", None, &[]);
        assert_eq!(keys, vec!["flywheel-toron".to_string()]);
        let keys = candidate_workspaces("flywheel-toron", Some("custom-ws"), &[]);
        assert!(keys.contains(&"flywheel-toron".to_string()));
        assert!(keys.contains(&"custom-ws".to_string()));
    }

    #[test]
    fn preferred_workspace_ignores_stale_meta() {
        let token = |l: &str| match l {
            "flywheel-toron" => "down",
            "adrx28" => "absent",
            _ => "absent",
        };
        assert_eq!(
            preferred_workspace("flywheel-toron", Some("adrx28"), token),
            "flywheel-toron"
        );
        let live = |l: &str| match l {
            "flywheel-toron" => "up",
            _ => "absent",
        };
        assert_eq!(
            preferred_workspace("flywheel-toron", Some("adrx28"), live),
            "flywheel-toron"
        );
    }

    #[test]
    fn hear_pubkey_clears_pending() {
        let mut h = HearTracker::default();
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        h.note_inject("PaleHarbor", Some(npub), 10);
        h.note_heard_pubkey(npub);
        assert!(h.unanswered_pins().is_empty());
        assert!(h.expire(200, UNANSWERED_SECS).is_empty());
    }

    #[test]
    fn hear_timeout_marks_unanswered() {
        let mut h = HearTracker::default();
        h.note_inject("PaleHarbor", None, 10);
        assert!(
            h.expire(10 + UNANSWERED_SECS - 1, UNANSWERED_SECS)
                .is_empty()
        );
        let expired = h.expire(10 + UNANSWERED_SECS, UNANSWERED_SECS);
        assert_eq!(expired, vec!["PaleHarbor".to_string()]);
        assert_eq!(h.unanswered_pins(), vec!["PaleHarbor".to_string()]);
    }

    #[test]
    fn hear_after_unanswered_clears() {
        let mut h = HearTracker::default();
        h.note_inject("PaleHarbor", None, 0);
        let _ = h.expire(UNANSWERED_SECS, UNANSWERED_SECS);
        h.note_heard_pin("PaleHarbor");
        assert!(h.unanswered_pins().is_empty());
    }

    #[test]
    fn hear_npub_filled_late_still_matches() {
        let mut h = HearTracker::default();
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        h.note_inject("PaleHarbor", None, 10);
        h.remember_npub("PaleHarbor", Some(npub));
        h.note_heard_pubkey(npub);
        assert!(h.expire(10 + UNANSWERED_SECS, UNANSWERED_SECS).is_empty());
        assert!(h.unanswered_pins().is_empty());
    }

    #[test]
    fn hear_late_post_clears_unanswered() {
        let mut h = HearTracker::default();
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        h.note_inject("PaleHarbor", Some(npub), 0);
        let _ = h.expire(UNANSWERED_SECS, UNANSWERED_SECS);
        assert_eq!(h.unanswered_pins(), vec!["PaleHarbor".to_string()]);
        h.note_heard_pubkey(npub);
        assert!(h.unanswered_pins().is_empty());
    }

    #[test]
    fn report_text_names_unanswered_pins() {
        let dir = std::env::temp_dir().join(format!("fw-hear-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("doorbell"));
        write_unanswered(
            &unanswered_file(&dir, "w1"),
            &["PaleHarbor".into(), "LiveWillow".into()],
        )
        .expect("write");
        let text = report_text(&dir, "w1");
        assert!(text.contains("dispatcher: down"), "{text}");
        assert!(
            text.contains("doorbell: unanswered pin=") && text.contains("PaleHarbor"),
            "{text}"
        );
        stop(&dir, "w1");
        assert!(read_unanswered(&unanswered_file(&dir, "w1")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_pubkey_from_live_shape() {
        let line = r#"{"kind":9,"id":"ab","pubkey":"AABBCC","tags":[],"content":"hi"}"#;
        assert_eq!(watch_pubkey(line).as_deref(), Some("aabbcc"));
    }

    // ---- bd-stale-doorbell-wake-0fic: watcher generation / self-retire ----

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fw-gen-{name}-{}", std::process::id()));
        let _ = fs::create_dir_all(d.join("doorbell"));
        d
    }

    #[test]
    fn gen_roundtrip_via_state_file() {
        let dir = scratch("rt");
        let path = gen_file(&dir, "w1");
        let record = WatcherGen {
            started_unix: 10,
            binary_mtime_unix: 5,
            binary_len: 77,
            binary_hash: String::new(),
        };
        write_gen(&path, &record).expect("write");
        assert_eq!(read_gen(&path), Some(record));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_binary_is_stale_and_refuses_inject() {
        let dir = scratch("chg");
        let exe = dir.join("fake-bin");
        fs::write(&exe, b"v1").expect("exe");
        let record = WatcherGen::capture(&exe, 100).expect("capture");
        assert!(!record.stale(&exe), "unchanged binary is live");
        assert!(inject_allowed(&record, &exe));
        // Rebuild: different length (and near-certainly mtime).
        fs::write(&exe, b"v2-longer").expect("rebuild");
        assert!(record.stale(&exe), "changed binary must read stale");
        assert!(
            !inject_allowed(&record, &exe),
            "acceptance: watcher older than its own binary must not inject"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn older_recorded_mtime_same_len_is_stale() {
        let dir = scratch("mt");
        let exe = dir.join("fake-bin-mtime");
        fs::write(&exe, b"abcd").expect("exe");
        let (mtime, len) = binary_identity(&exe).expect("identity");
        let older = WatcherGen {
            started_unix: 0,
            binary_mtime_unix: mtime.max(1) - 1,
            binary_len: len,
            binary_hash: String::new(),
        };
        assert!(older.stale(&exe));
        assert!(!inject_allowed(&older, &exe));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_binary_fails_closed_no_inject() {
        let record = WatcherGen {
            started_unix: 0,
            binary_mtime_unix: 1,
            binary_len: 1,
            binary_hash: String::new(),
        };
        assert!(record.stale(Path::new("/nonexistent/flywheel-bin")));
        assert!(!inject_allowed(
            &record,
            Path::new("/nonexistent/flywheel-bin")
        ));
    }

    #[test]
    fn banner_stamp_carries_up_and_bin_mtime() {
        let record = WatcherGen {
            started_unix: 1700,
            binary_mtime_unix: 1600,
            binary_len: 1,
            binary_hash: String::new(),
        };
        let line = stamp_line(
            &doorbell_line(EventClass::RequestRelease, "toron mail inbox --since"),
            &record,
        );
        assert!(line.starts_with("[doorbell] request-release."), "{line}");
        assert!(line.contains("Pull: toron mail inbox --since"), "{line}");
        assert!(line.contains("watcher up=1700"), "{line}");
        assert!(line.contains("bin_mtime=1600"), "{line}");
    }

    #[test]
    fn report_text_carries_generation_when_present() {
        let dir = scratch("rep");
        let record = WatcherGen {
            started_unix: 1234,
            binary_mtime_unix: 1200,
            binary_len: 9,
            binary_hash: String::new(),
        };
        write_gen(&gen_file(&dir, "w9"), &record).expect("gen");
        let text = report_text(&dir, "w9");
        assert!(text.contains("dispatcher:"), "{text}");
        assert!(
            text.contains("watcher up=1234") && text.contains("bin_mtime=1200"),
            "{text}"
        );
        assert_eq!(
            report_text(&dir, "w10").lines().next(),
            Some("dispatcher: down"),
            "absent gen keeps the bare shape"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_generation_supersedes_live_pre_fix_watcher() {
        let dir = scratch("lock");
        let pid = pid_file(&dir, "ws");
        write_pid(&pid, 424_242).expect("pid");
        let gpath = gen_file(&dir, "ws");
        let cur = Some((50u64, 10u64));
        assert_eq!(
            inspect_lock_generation(&pid, &gpath, cur, |_| true),
            LockAction::Superseded { pid: 424_242 },
            "alive pid with no gen file = pre-fix watcher, replace it"
        );
        write_gen(
            &gpath,
            &WatcherGen {
                started_unix: 40,
                binary_mtime_unix: 50,
                binary_len: 10,
                binary_hash: String::new(),
            },
        )
        .expect("gen");
        assert_eq!(
            inspect_lock_generation(&pid, &gpath, cur, |_| true),
            LockAction::Attach { pid: 424_242 }
        );
        write_gen(
            &gpath,
            &WatcherGen {
                started_unix: 40,
                binary_mtime_unix: 49,
                binary_len: 10,
                binary_hash: String::new(),
            },
        )
        .expect("gen");
        assert_eq!(
            inspect_lock_generation(&pid, &gpath, cur, |_| true),
            LockAction::Superseded { pid: 424_242 },
            "rebuilt binary supersedes even with a gen file present"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_clears_generation_file() {
        let dir = scratch("stop");
        write_gen(
            &gen_file(&dir, "wst"),
            &WatcherGen {
                started_unix: 1,
                binary_mtime_unix: 1,
                binary_len: 1,
                binary_hash: String::new(),
            },
        )
        .expect("gen");
        stop(&dir, "wst");
        assert!(read_gen(&gen_file(&dir, "wst")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- bd-durable-doorbell-lngf: durable wake state + heal scope --------

    /// Wake state round-trips through disk: since_by_pin high-waters + the
    /// last-wake stamp survive a dispatcher crash (at-least-once redelivery
    /// reads the same file on restart).
    #[test]
    fn wake_state_roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!("dbllngf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = wake_state_file(&dir, "wst");
        let mut st = WakeState::default();
        st.since_by_pin
            .insert("QuietFjord".into(), "2026-08-23T01:00:00Z".into());
        st.since_by_pin
            .insert("IronHarbor".into(), "2026-08-23T02:00:00Z".into());
        note_wake(&mut st, "QuietFjord", 1_787_440_000);
        save_wake_state(&path, &st).expect("save");

        let back = load_wake_state(&path);
        assert_eq!(
            back.since_by_pin.get("QuietFjord").map(String::as_str),
            Some("2026-08-23T01:00:00Z")
        );
        assert_eq!(back.last_wake_unix, Some(1_787_440_000));
        assert_eq!(back.last_wake_pin.as_deref(), Some("QuietFjord"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-7qg6: one writer, one ratchet — a same-second or behind stamp is
    /// refused so rapid double prompts cannot fake liveness for `send --wait`.
    #[test]
    fn note_wake_refuses_non_advancing_stamp() {
        let mut st = WakeState::default();
        assert!(note_wake(&mut st, "A", 100));
        assert_eq!(st.last_wake_unix, Some(100));
        assert_eq!(st.last_wake_pin.as_deref(), Some("A"));
        assert!(
            !note_wake(&mut st, "B", 100),
            "same-second re-note must not bump"
        );
        assert!(
            !note_wake(&mut st, "B", 99),
            "behind stamp must not clobber"
        );
        assert_eq!(st.last_wake_unix, Some(100));
        assert_eq!(st.last_wake_pin.as_deref(), Some("A"));
        assert!(note_wake(&mut st, "B", 101));
        assert_eq!(st.last_wake_pin.as_deref(), Some("B"));
    }

    /// A missing/corrupt wake state file loads as DEFAULT (never a crash) —
    /// at-least-once degrades to re-delivery, which is idempotent.
    #[test]
    fn load_wake_state_defaults_on_missing_or_corrupt() {
        let dir = std::env::temp_dir().join(format!("dbllngf-miss-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let st = load_wake_state(&wake_state_file(&dir, "nope"));
        assert!(st.since_by_pin.is_empty());
        assert!(st.last_wake_unix.is_none());
        let corrupt = dir.join("corrupt.json");
        std::fs::write(&corrupt, "{not json").unwrap();
        assert!(load_wake_state(&corrupt).since_by_pin.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Heal scope: the dispatcher key is the workspace LABEL (the pid/gen
    /// file stem `swarm_label` writes), NEVER the herdr pane-id prefix —
    /// a label-keyed heal finds the real pid files (bd-durable-doorbell-
    /// lngf graduation finding: w8B vs flywheel-flywheel).
    #[test]
    fn wake_state_file_uses_workspace_label_key() {
        let dir = std::env::temp_dir().join(format!("dbllngf-label-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = wake_state_file(&dir, "flywheel-flywheel");
        assert!(
            path.to_string_lossy()
                .contains("doorbell-wake-flywheel-flywheel")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gs006_mail_delivered_kind9_is_mail_not_channel() {
        // NIP-17 gift-wrap wake: kind 9 + toron-kind=mail-delivered must pull mail, not channel.
        let line = r#"{"kind":9,"id":"aa","tags":[["p","BlueRidge"],["t","flywheel"],["toron-kind","mail-delivered"],["h","thread-123"]],"content":"sealed"}"#;
        let (class, hint) = parse_watch_ndjson(line, &SelfIndex::from_pins(["BlueRidge"]))
            .expect("mail-delivered must ring");
        assert_ne!(
            class,
            EventClass::Channel,
            "mail-delivered must not be Channel"
        );
        assert_eq!(
            class,
            EventClass::PlanePMail,
            "mail-delivered is routine inbox-pull, urgency comes from the fold"
        );
        assert!(
            hint.contains("toron mail inbox"),
            "hint must be mail inbox, got {hint}"
        );
    }

    /// bd-pe5o: live kind-9 channel-post frame contract. A real
    /// `channel post --mention` frame (h channel tag + t project tag +
    /// p npub-hex tag + @Pin content, exactly as toron emits) must ring
    /// Channel for the mentioned pin. Tripwire for the post-to-wake path:
    /// when wakes stop arriving, this test says whether the flywheel half
    /// still holds its end of the seam.
    #[test]
    fn doorbell_wake_kind9_mention_post_rings_channel() {
        let mut index = SelfIndex::default();
        index.insert_pin(
            "BrightBadger".into(),
            Some("699d97318e69efb667767a917972bebf396d63a10bd6d68bec01e698a903b746".into()),
        );
        // Exact shape of a verified live post (event 17baed1e).
        let line = r#"{"kind":9,"id":"17baed1e20b74f424c1c2117bdf7b0b334d928bec923b7194c46cf33116b8b49","tags":[["h","steer-flywheel"],["t","flywheel"],["p","699d97318e69efb667767a917972bebf396d63a10bd6d68bec01e698a903b746"]],"content":"@BrightBadger wake probe4 bd-pe5o"}"#;
        let (class, _) = parse_watch_ndjson(line, &index).expect("mention post must ring");
        assert_eq!(class, EventClass::Channel);
        // Content-only path (no p tag): @mention in the body still addresses.
        let bare = r#"{"kind":9,"id":"cc","tags":[["h","steer-flywheel"],["t","flywheel"]],"content":"@BrightBadger ping"}"#;
        let (class, _) = parse_watch_ndjson(bare, &index).expect("@mention must ring");
        assert_eq!(class, EventClass::Channel);
        // Unknown pin, no mention: room chatter stays silent.
        let chatter = r#"{"kind":9,"id":"dd","tags":[["h","steer-flywheel"],["t","flywheel"]],"content":"hello room"}"#;
        assert!(parse_watch_ndjson(chatter, &index).is_none());
    }

    #[test]
    fn gs006_kind39002_join_leave_rings_when_addressed() {
        // 39002 join/leave: when p-tag addresses self, doorbell must ring (subscribe + classify).
        let line = r#"{"kind":39002,"id":"bb","tags":[["p","BlueRidge"],["t","flywheel"]],"content":"join"}"#;
        let (class, _) = parse_watch_ndjson(line, &SelfIndex::from_pins(["BlueRidge"]))
            .expect("39002 must ring when addressed");
        assert!(
            matches!(
                class,
                EventClass::Channel
                    | EventClass::Job
                    | EventClass::PlanePUrgent
                    | EventClass::PlanePMail
                    | EventClass::RequestRelease
                    | EventClass::ReservationConflict
            ),
            "39002 addressed should map to a wake class, got {class:?}"
        );
    }

    /// bd-yw4s: journal_append + journal_read_all roundtrip — `doorbell why`
    /// replays this; corrupt lines are skipped, never fatal.
    #[test]
    fn doorbell_journal_roundtrip_and_corrupt_tolerant() {
        let dir = std::env::temp_dir().join(format!("dbljrn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let msg = crate::doorbell_tick::DoorbellMsg::HerdrStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        };
        let actions = vec![crate::doorbell_tick::DoorbellAction::Prompt {
            pin: "BlueRidge".into(),
            line: "[doorbell] test".into(),
        }];
        journal_append(
            &dir,
            "w1",
            &msg,
            &actions,
            serde_json::json!({"stage":"drain"}),
        );
        // corrupt line must be skipped, not fatal
        let path = journal_file(&dir, "w1");
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{{not json").unwrap();
        journal_append(
            &dir,
            "w2",
            &msg,
            &actions,
            serde_json::json!({"stage":"drain"}),
        );
        let all = journal_read_all(&dir);
        assert_eq!(all.len(), 2, "two good entries survive the corrupt line");
        assert_eq!(all[0].0, "w1");
        assert_eq!(all[1].1["msg"]["HerdrStatus"]["pin"], "BlueRidge");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-swad: `why` is the trust surface, so the tear line it prints has to
    /// answer "how bad" without a second command.
    ///
    /// The numbers existed in the LoudMiss audit row (bd-9ase) and in
    /// /health's torn_tails (bd-5qyt), and neither is the line an operator
    /// reads first. Rendered through `reason` alone they were prose, present
    /// only for the seq-gap case, and silent about rows_withheld in every case
    /// — so a 16-row hole and the 5,175-row one in flywheel-chiebukuro read the
    /// same.
    #[test]
    fn why_names_the_tear_size_and_what_it_hid() {
        let dir = std::env::temp_dir().join(format!("dblwhy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(dir.join("journals"));
        // Contiguous through 3, then 10: six values (4..=9) never written, and
        // the row AT the tear plus everything after it is unreachable from here.
        let mut body = String::new();
        for seq in [1u64, 2, 3, 10, 11] {
            body.push_str(&format!(
                "{{\"v\":2,\"seq\":{seq},\"actions\":[],\"executed\":{{}}}}\n"
            ));
        }
        std::fs::write(journal_file(&dir, "probe"), body).expect("write journal");

        let (torn, first_seq) = journal_fold_status(&dir, "probe");
        let line = torn.expect("a six-seq hole is a tear, and silence is the bug");
        assert!(
            line.contains("missing=6"),
            "the line must carry the hole's SIZE, not just that one exists: {line}"
        );
        assert!(
            line.contains("range=4..=9"),
            "and WHICH seqs are gone, so another surface is not needed: {line}"
        );
        assert!(
            line.contains("withheld=2"),
            "and how much history the stop makes unreachable (the row at the tear \
             plus the one after it): {line}"
        );
        assert_eq!(first_seq, Some(1), "the foldable head is unchanged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_v1_v2_fold_and_v3_refusal() {
        // v1 (legacy, no seq) and v2 (subscriber + budget rows) both fold;
        // v3+ refuses loudly (bd-asr2.9 D4.1).
        assert!(journal_version_accepted(&serde_json::json!({})));
        assert!(journal_version_accepted(&serde_json::json!({"v": 1})));
        assert!(journal_version_accepted(&serde_json::json!({"v": 2})));
        assert!(!journal_version_accepted(&serde_json::json!({"v": 3})));
        // Mixed v1+v2 lines fold together through the store.
        let dir = std::env::temp_dir().join(format!("dbll-v12-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = doorbell_rt::FileStore::new(&dir);
        store
            .append_journal(
                "w",
                &serde_json::json!({"rt_kind": "Legacy"}),
                &serde_json::json!([]),
                serde_json::json!({"stage": "sweep"}),
                &[],
            )
            .unwrap();
        let fold = store.fold_workspace("w");
        assert_eq!(fold.rows.len(), 1, "v1 legacy row folds");
        assert!(fold.torn.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roster_scoped_rows_and_render() {
        // E2 shape: P's digest carries Q's row (scope-bound roster).
        let pins = vec![
            ("P".to_string(), "pi".to_string()),
            ("Q".to_string(), "pi".to_string()),
        ];
        let mut roles = BTreeMap::new();
        roles.insert("Q".to_string(), "coder".to_string());
        let presence = vec![("Q".to_string(), "working".to_string(), 900u64)];
        let mut beads = BTreeMap::new();
        beads.insert("Q".to_string(), "bd-1".to_string());
        let rows = build_roster(&pins, &roles, &presence, &beads, 1000);
        assert_eq!(rows.len(), 2);
        // P (no presence): unknown, never invent-green.
        assert_eq!(rows[0].state, "unknown");
        assert_eq!(rows[0].recency_secs, None);
        assert_eq!(rows[0].role, "");
        // Q carries everything the digest needs.
        assert_eq!(rows[1].role, "coder");
        assert_eq!(rows[1].state, "working");
        assert_eq!(rows[1].recency_secs, Some(100));
        assert_eq!(rows[1].bead, "bd-1");
        let rendered = render_roster(&rows);
        assert!(rendered[0].starts_with("ROSTER"));
        assert!(rendered[2].contains('Q') && rendered[2].contains("bd-1"));
    }

    #[test]
    fn nearby_join_keeps_unbound_panes_and_holds() {
        // BoardAgent had no registry pin — nearby must still list the pane.
        let registry = vec![(
            "QuietHeron".to_string(),
            "wP:p2".to_string(),
            "grok".to_string(),
        )];
        let herdr = vec![
            (
                "wP:p2".to_string(),
                "grok".to_string(),
                "working".to_string(),
            ),
            (
                "w6:p2".to_string(),
                "BoardAgent".to_string(),
                "idle".to_string(),
            ),
        ];
        let leases = vec![("QuietHeron".to_string(), "src/main.rs".to_string())];
        let rows = join_nearby(&registry, &herdr, &leases);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let bound = rows.iter().find(|r| r.pin == "QuietHeron").expect("bound");
        assert_eq!(bound.pane, "wP:p2");
        assert_eq!(bound.state, "working");
        assert_eq!(bound.held, "src/main.rs");
        let unbound = rows
            .iter()
            .find(|r| r.name == "BoardAgent")
            .expect("unbound");
        assert!(unbound.pin.is_empty(), "unbound pane has no pin");
        assert_eq!(unbound.pane, "w6:p2");
        let rendered = render_nearby(&rows);
        assert!(rendered[0].starts_with("NEARBY"));
        assert!(rendered.iter().any(|l| l.contains("BoardAgent")));
        assert!(rendered.iter().any(|l| l.contains("held=src/main.rs")));
    }

    #[test]
    fn introduce_steps_are_contact_then_channel_then_subscribe() {
        let steps = introduce_steps(
            "flywheel",
            "BoardAgent",
            "QuietHeron",
            "intro-boardagent",
            "flywheel-flywheel",
        );
        assert_eq!(
            steps.iter().map(|s| s.verb).collect::<Vec<_>>(),
            ["contact", "channel", "subscribe"]
        );
        assert!(
            steps[0]
                .argv
                .windows(2)
                .any(|w| w == ["--to", "BoardAgent"]),
            "contact requests the peer: {:?}",
            steps[0].argv
        );
        assert!(
            steps[1].argv.iter().any(|a| a == "intro-boardagent"),
            "channel create uses the intro id: {:?}",
            steps[1].argv
        );
        assert!(
            steps[2]
                .argv
                .windows(2)
                .any(|w| w == ["--pin", "BoardAgent"])
                && steps[2]
                    .argv
                    .windows(2)
                    .any(|w| w == ["--project", "flywheel"]),
            "subscribe names pin+project: {:?}",
            steps[2].argv
        );
        assert_eq!(introduce_channel_id("BoardAgent"), "intro-boardagent");
    }

    #[test]
    fn introduce_idempotent_does_not_swallow_missing_peer() {
        assert!(introduce_idempotent("channel already exists"));
        assert!(introduce_idempotent("already a member"));
        assert!(introduce_idempotent("idempotent bootstrap"));
        assert!(
            !introduce_idempotent("agent BoardAgent does not exist"),
            "missing peer is a hard fail, not success"
        );
        assert!(!introduce_idempotent("project flywheel does not exist"));
    }

    #[test]
    fn cron_tick_wake_journals_fact_row() {
        // bd-asr2.16: the sink writes a fact_pending journal row (plus a
        // dispatcher pending-fact entry — same helper production arms use).
        let dir = std::env::temp_dir().join(format!("dbll-cron-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let msg = crate::workspace::Msg::Fact {
            summary: "cron nightly tick t1 completed".into(),
        };
        journal_append_workspace(
            &dir,
            "proj",
            &msg,
            &[],
            serde_json::json!({ "stage": "cron_tick", "summary": "x" }),
            None,
        );
        let rows = doorbell_rt::FileStore::new(&dir)
            .fold_workspace("proj")
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["executed"]["stage"], serde_json::json!("cron_tick"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cron_sink_flag_off_writes_nothing() {
        // bd-asr2.16: without the flag, tick completion is a silent no-op.
        // Hermetic home (the operator's real config may carry
        // cron_doorbell_sink = true, which made this env-dependent): a
        // fresh XDG config+state pair parses with the flag at its
        // default-off, so the gate must return before any write path.
        let _home = crate::config::test_state_home();
        let cfg_dir = crate::config::config_dir();
        std::fs::create_dir_all(&cfg_dir).expect("cfg dir");
        std::fs::write(
            crate::config::config_file(),
            "[doorbell]\nbeads_poller = false\ncron_doorbell_sink = false\n",
        )
        .expect("write hermetic config");
        let listed = || -> Vec<String> {
            std::fs::read_dir(crate::config::state_dir().join("journals"))
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        let before = listed();
        cron_tick_wake("flagprobe-ws", "nightly", "t1", "completed");
        assert_eq!(before, listed(), "flag-off tick must not touch journals");
    }

    fn emit_services() -> std::collections::HashMap<String, crate::config::ServiceEmit> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"tok-ci-1");
        let hex = format!("{:x}", h.finalize());
        let mut m = std::collections::HashMap::new();
        m.insert(
            "ci".to_string(),
            crate::config::ServiceEmit {
                token_sha256: hex,
                channels: vec!["captain".to_string()],
                rate_per_min: 6,
            },
        );
        m
    }

    #[test]
    fn emit_authorize_matrix() {
        // bd-asr2.17: ok / unknown service / bad token / scope deny.
        let m = emit_services();
        assert_eq!(authorize_emit(&m, "ci", "tok-ci-1", "captain"), Ok(6));
        assert_eq!(
            authorize_emit(&m, "nope", "tok-ci-1", "captain"),
            Err(EmitDeny::UnknownService)
        );
        assert_eq!(
            authorize_emit(&m, "ci", "wrong", "captain"),
            Err(EmitDeny::BadToken)
        );
        assert_eq!(
            authorize_emit(&m, "ci", "tok-ci-1", "someone-else"),
            Err(EmitDeny::ScopeDenied)
        );
    }

    #[test]
    fn emit_revoked_service_refused() {
        // bd-asr2.17: revocation = entry deleted -> UnknownService.
        let mut m = emit_services();
        m.remove("ci");
        assert_eq!(
            authorize_emit(&m, "ci", "tok-ci-1", "captain"),
            Err(EmitDeny::UnknownService)
        );
    }

    #[test]
    fn emit_wake_journals_service_row() {
        // bd-asr2.17: admitted emit leaves a stage=emit Fact row.
        let dir = std::env::temp_dir().join(format!("dbll-emit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        service_emit_wake_in(&dir, "proj", "ci", "captain");
        let rows = doorbell_rt::FileStore::new(&dir)
            .fold_workspace("proj")
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["executed"]["stage"], serde_json::json!("emit"));
        assert_eq!(rows[0]["executed"]["service"], serde_json::json!("ci"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seed_home_sub(dir: &std::path::Path, ws: &str, pin: &str) {
        use doorbell_rt::subscribe::*;
        let mut store = SubscriptionStore::open(dir);
        let sub = Subscription {
            id: format!("spawn-{ws}-{pin}"),
            identity: Identity::Pin(pin.to_string()),
            scope: Scope::One(ws.to_string()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
            project: Some("proj".into()),
            endpoint: None,
        };
        store
            .subscribe(Actor::local_process(), sub, ws)
            .expect("seed home sub");
    }

    #[test]
    fn cross_widen_many_deduped() {
        // bd-asr2.18: explicit extra workspace -> Scope::Many, deduped.
        let dir = std::env::temp_dir().join(format!("dbll-xw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        seed_home_sub(&dir, "wA", "P");
        let ws = subscribe_extra_workspace_in(&dir, "wA", "P", "wB", true, true).expect("widen");
        assert_eq!(ws, vec!["wA".to_string(), "wB".to_string()]);
        let ws2 =
            subscribe_extra_workspace_in(&dir, "wA", "P", "wB", true, true).expect("re-widen");
        assert_eq!(ws2.len(), 2, "dedupe: no second wB");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_widen_refusals() {
        // bd-asr2.18: flag off / non-operator / missing sub refuse.
        let dir = std::env::temp_dir().join(format!("dbll-xwref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        seed_home_sub(&dir, "wA", "P");
        assert_eq!(
            subscribe_extra_workspace_in(&dir, "wA", "P", "wB", true, false),
            Err(CrossScopeDeny::FlagOff)
        );
        assert_eq!(
            subscribe_extra_workspace_in(&dir, "wA", "P", "wB", false, true),
            Err(CrossScopeDeny::NeedsOperator)
        );
        assert_eq!(
            subscribe_extra_workspace_in(&dir, "wA", "NOBODY", "wB", true, true),
            Err(CrossScopeDeny::NoSubscription)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn inject_cleanup(proj: &str) {
        let pend = crate::config::state_dir()
            .join("facts")
            .join(format!("{}-pending.jsonl", crate::state::state_key(proj)));
        let _ = std::fs::remove_file(pend);
    }

    #[test]
    fn inject_journals_row_and_queues_wake() {
        // D8/bd-woha: console inject lands a stage=inject Fact row in the
        // named scope journal plus one pending wake. Real pending queue
        // (fake project), temp journal — cleaned after.
        let dir = std::env::temp_dir().join(format!("dbll-inj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = "inject-probe";
        let out = inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_000).expect("inject");
        assert!(out.contains('P') && out.contains("hello"));
        let rows = doorbell_rt::FileStore::new(&dir)
            .fold_workspace("manual")
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["executed"]["stage"], serde_json::json!("inject"));
        assert_eq!(rows[0]["executed"]["pin"], serde_json::json!("P"));
        let pend = crate::config::state_dir()
            .join("facts")
            .join(format!("{}-pending.jsonl", crate::state::state_key(proj)));
        let body = std::fs::read_to_string(&pend).unwrap_or_default();
        assert!(body.contains("hello"), "one wake queued");
        inject_cleanup(proj);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inject_ratchet_refuses_same_second_dup() {
        // D8: identical pin+line within one unix second refuses.
        let dir = std::env::temp_dir().join(format!("dbll-injr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = "inject-probe-r";
        inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_000).expect("first");
        assert_eq!(
            inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_000),
            Err(InjectDeny::Ratchet)
        );
        // Next second: admitted (budget still has room).
        inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_001).expect("next second");
        inject_cleanup(proj);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inject_budget_denies_fourth_in_window() {
        // D8: 3 per 1800s rolling; the 4th refuses, spend is persisted.
        let dir = std::env::temp_dir().join(format!("dbll-injb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = "inject-probe-b";
        for i in 0..3 {
            inject_wake_in(
                &dir,
                proj,
                "manual",
                "P",
                &format!("m{i}"),
                1_700_000 + i * 10,
            )
            .expect("within budget");
        }
        assert_eq!(
            inject_wake_in(&dir, proj, "manual", "P", "m3", 1_700_030),
            Err(InjectDeny::Budget)
        );
        // Window rolls: t+1801 admits again.
        inject_wake_in(&dir, proj, "manual", "P", "later", 1_700_000 + 1801).expect("rolled");
        inject_cleanup(proj);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// governed-by: bd-z7w0 — resolve_host Embedded arms reachability.
    /// Dead configured URL → TakeLease embedded (+ loud WARNING).
    /// KEEP: fires whenever the configured daemon is down.
    #[test]
    fn degraded_audit_resolve_host_embedded_on_dead_url() {
        let dir = std::env::temp_dir().join(format!("dbll-audit-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Bind-drop: guaranteed RST, no timing flake.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let host = resolve_host(
            &dir,
            Some(format!("http://127.0.0.1:{port}")),
            Some(("proj", "ws")),
        );
        assert_eq!(host, DoorbellHost::Embedded, "dead daemon URL embeds");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// governed-by: bd-z7w0 — RefuseDoubleRun arm reachability. A live
    /// lease held by another writer refuses the second loop (embedded).
    /// KEEP: fires on every concurrent-loop attempt.
    #[test]
    fn degraded_audit_resolve_host_refuses_double_run() {
        use doorbell_rt::lease::{OwnerLease, owner_file};
        let dir = std::env::temp_dir().join(format!("dbll-audit-lease-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("doorbell")).unwrap();
        let now = doorbell_rt::lease::unix_now();
        let lease = OwnerLease {
            pid: std::process::id(),
            boot_id: "audit".into(),
            host: "audit".into(),
            started_unix: now,
            heartbeat_unix: now,
        };
        std::fs::write(
            owner_file(&dir, "ws"),
            serde_json::to_string(&lease).unwrap(),
        )
        .unwrap();
        let host = resolve_host(&dir, None, Some(("proj", "ws")));
        assert_eq!(
            host,
            DoorbellHost::Embedded,
            "live foreign lease refuses the second loop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod bd_2zqk_health_probe_tests {
    use super::{
        HEALTH_CONNECT_TIMEOUT_MS, HEALTH_READ_TIMEOUT_MS, HealthProbeFailure, daemon_health_probe,
    };

    /// bd-2zqk: the budgets must clear the daemon's measured latency tail. The
    /// old 200ms/500ms pair sat ON the tail (measured max 506ms on a ~1.4KB
    /// body) and reported 4 of 12 healthy daemons as unreachable. This row pins
    /// the ordering, not the exact numbers, so a future tweak cannot shrink the
    /// budget back under the tail.
    #[test]
    fn the_probe_budget_clears_the_measured_daemon_tail() {
        let measured_tail_ms: u64 = 506; // observed max latency of /health
        // 3x the observed tail is the margin. Not 4x: this sits on the session
        // door, so the worst case has to stay bounded for an operator waiting on
        // `arrive`, and 2s already clears every latency we have actually
        // measured. Do not shrink it, and do not inflate the production timeout
        // just to satisfy a threshold — raise the bar here only with evidence.
        assert!(
            HEALTH_READ_TIMEOUT_MS >= 3 * measured_tail_ms,
            "read budget {HEALTH_READ_TIMEOUT_MS}ms must clear 3x the measured {measured_tail_ms}ms \
             tail, or a busy daemon reads as dead again"
        );
        assert!(
            HEALTH_CONNECT_TIMEOUT_MS > measured_tail_ms,
            "connect budget {HEALTH_CONNECT_TIMEOUT_MS}ms must clear the tail too"
        );
    }

    /// The whole point of the enum: a daemon that is LISTENING but slow must not
    /// be described as unreachable, because the remediation for unreachable
    /// ("start a daemon") is actively wrong — one is already running.
    #[test]
    fn a_slow_but_live_daemon_is_never_reported_as_unreachable() {
        let e = HealthProbeFailure::ReadTimedOut;
        let reason = e.reason();
        assert!(
            reason.contains("NOT a dead daemon") && reason.contains("do not start a second one"),
            "the reason must refuse the restart advice: {reason}"
        );
        assert!(
            !reason.contains("unreachable"),
            "must not use the word that sends the operator to start a second daemon: {reason}"
        );
    }

    /// One row per failure mode. Each must be distinguishable in the message,
    /// because the operator action differs: only NotListening means "start one".
    #[test]
    fn every_failure_mode_names_its_own_cause() {
        let rows = [
            (HealthProbeFailure::UnparseableUrl, "host:port"),
            (HealthProbeFailure::NotListening, "nothing is listening"),
            (HealthProbeFailure::ReadTimedOut, "accepted the connection"),
            (HealthProbeFailure::BadPayload, "non-JSON"),
            (HealthProbeFailure::ProtocolMismatch, "not doorbell.v1"),
        ];
        for (failure, must_contain) in rows {
            let reason = failure.reason();
            assert!(
                reason.contains(must_contain),
                "{failure:?} must mention {must_contain:?}: {reason}"
            );
        }
    }

    /// A wrong port must report NotListening (so "start one" is still correct),
    /// and must not hang: the connect budget is bounded.
    #[test]
    fn a_dead_port_reports_not_listening_promptly() {
        let started = std::time::Instant::now();
        // Port 1 is reserved and nothing listens on it.
        let err = daemon_health_probe("http://127.0.0.1:1").expect_err("must fail");
        assert_eq!(err, HealthProbeFailure::NotListening);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(HEALTH_CONNECT_TIMEOUT_MS * 2),
            "a refused connection must return fast, not burn the whole budget"
        );
    }

    /// An unparseable url is its own mode, so it never masquerades as a dead
    /// daemon (which would send the operator to start one).
    #[test]
    fn an_unparseable_url_is_its_own_failure_mode() {
        assert_eq!(
            daemon_health_probe("not-a-url").expect_err("must fail"),
            HealthProbeFailure::UnparseableUrl
        );
    }
}

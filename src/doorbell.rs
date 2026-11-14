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

#[cfg(any(test, feature = "serve"))]
pub use doorbell_core::PinBudget;
#[cfg(test)]
pub use doorbell_core::{
    DoorbellQueue, EventClass, RingInput, UNANSWERED_SECS, classify, doorbell_line,
    inbox_high_water, is_drain_transition, parse_inbox_item, parse_rfc3339_micros,
    parse_watch_ndjson, should_inject, watch_pubkey, watch_targets,
};
pub use doorbell_core::{SelfIndex, WakeState, note_wake, parse_status_pair};

/// The ONE inbox-lease key for a doorbell host (rv23 S1/S2).
///
/// A doorbell host is spawned as `__doorbell --workspace <label>` and keys
/// every file and lease by that hc workspace LABEL, never by the hc
/// workspace id (`w1`). The inbox host leases this key and the sender looks
/// up this key, so the two cannot drift: when the sender looked up
/// `inbox-<workspace id>` it never found the host's `inbox-<label>` lease and
/// rang the pane itself, a second unbudgeted wake per send (22 sends + 5 deaf
/// rings gave 53 pane writes).
pub(crate) fn inbox_owner_key(label: &str) -> String {
    doorbell_rt::lease::inbox_lease_key(label)
}

/// Pid file: `~/.local/state/flywheel/doorbell/<workspace>.pid`.
pub fn pid_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir.join("doorbell").join(format!("{workspace}.pid"))
}

/// ADR-0005 D1.4 first line: `dispatcher: up|down …`
pub fn dispatcher_status_line(state_dir: &Path, workspace: &str) -> String {
    match read_state(state_dir, workspace).and_then(|state| state.pid) {
        Some(pid) if pid_alive(pid) => format!("dispatcher: up pid={pid}"),
        Some(pid) => format!("dispatcher: down stale={pid}"),
        None => "dispatcher: down".into(),
    }
}

/// Helm/snapshot token: `absent` (never started) vs `down` (dead) vs `up`.
/// Snapshot must not treat lowercase `down` as absent.
pub fn dispatcher_token(state_dir: &Path, workspace: &str) -> &'static str {
    if let Some(state) = read_state(state_dir, workspace)
        && (state.pid.is_some() || state.project.is_some() || state.generation.is_some())
    {
        return if state.pid.is_some_and(pid_alive) {
            "up"
        } else {
            "down"
        };
    }
    "absent"
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

pub fn read_pid(path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse().ok()
}

#[cfg(test)]
pub fn write_pid(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    fs::write(path, format!("{pid}\n")).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
pub fn clear_pid(path: &Path) {
    let _ = fs::remove_file(path);
}

pub fn pid_alive(pid: u32) -> bool {
    crate::state::pid_alive(pid)
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

/// One authoritative static-state record for a workspace: pid, project and
/// watcher generation. The old pid/gen/project sidecars remain readable for
/// one migration window, but every new write lands here first.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DoorbellState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<WatcherGen>,
}

/// Canonical dispatcher key for `ws`: the live session id when `ws` names
/// this workspace by its label, else `ws` itself. One workspace, one key,
/// one door (bd-g9hqu) — the label and its id are the same host.
pub fn canonical_workspace<'a>(ws: &'a str, live_label: &str, live_id: &'a str) -> &'a str {
    if !live_id.is_empty() && (live_label.is_empty() || live_label == ws) {
        live_id
    } else {
        ws
    }
}

/// Every dispatcher record for one project: (workspace, pid). bd-bj9xk:
/// `doorbell status` listed one row per canonical workspace, so duplicate
/// doors — including the dead sessions — were invisible.
pub fn project_dispatchers(state_dir: &Path, project: &str) -> Vec<(String, Option<u32>)> {
    let dir = state_dir.join("doorbell");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(ws) = name.strip_suffix(".state.json") else {
            continue;
        };
        let Some(state) = read_state(state_dir, ws) else {
            continue;
        };
        if state.project.as_deref() != Some(project) {
            continue;
        }
        out.push((ws.to_string(), state.pid));
    }
    out.sort();
    out
}

/// Canonical project key: a filesystem path names the same project as its
/// directory slug (`…/workspace/chiebukuro` == `chiebukuro`), so a
/// path-valued `--project` never starts a second dispatcher for one project
/// (bd-1mi26). Pure and total: a bare slug is already canonical.
pub fn canonical_project(project: &str) -> String {
    let trimmed = project.trim();
    if !trimmed.contains(std::path::MAIN_SEPARATOR) && !trimmed.contains('/') {
        return trimmed.to_string();
    }
    std::path::Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| trimmed.to_string())
}

/// `workspaces` collapsed to their canonical dispatcher keys, order kept,
/// first occurrence wins. Every register/start/stop/heal/report path folds
/// through this so a label and its session id never act as two workspaces.
pub fn canonical_workspaces(workspaces: &[String], live_label: &str, live_id: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for ws in workspaces {
        let key = canonical_workspace(ws, live_label, live_id).to_string();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// `<workspace>.state.json`, beside the legacy pid/gen/project sidecars.
pub fn state_file(state_dir: &Path, workspace: &str) -> PathBuf {
    doorbell_rt::lease::state_file(state_dir, workspace)
}

pub fn read_state(state_dir: &Path, workspace: &str) -> Option<DoorbellState> {
    let raw = fs::read_to_string(state_file(state_dir, workspace)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Live dispatcher pid from the unified state record.
pub fn dispatcher_pid(state_dir: &Path, workspace: &str) -> Option<u32> {
    read_state(state_dir, workspace).and_then(|state| state.pid)
}

/// Project recorded by the unified state record.
pub fn dispatcher_project(state_dir: &Path, workspace: &str) -> Option<String> {
    read_state(state_dir, workspace).and_then(|state| state.project)
}

/// Watcher generation recorded by the unified state record.
pub fn dispatcher_generation(state_dir: &Path, workspace: &str) -> Option<WatcherGen> {
    read_state(state_dir, workspace).and_then(|state| state.generation)
}

fn inspect_state_generation(
    state: Option<&DoorbellState>,
    current_identity: Option<(u64, u64)>,
    current_hash: Option<String>,
    alive: impl Fn(u32) -> bool,
) -> LockAction {
    let Some(state) = state else {
        return LockAction::Start;
    };
    let Some(pid) = state.pid else {
        return LockAction::Start;
    };
    if !alive(pid) {
        return LockAction::Reloaded { stale: pid };
    }
    let (Some(record), Some((mtime, len))) = (state.generation.as_ref(), current_identity) else {
        return LockAction::Superseded { pid };
    };
    let mtime_ok = record.matches(mtime, len);
    let hash_ok = if record.binary_hash.is_empty() {
        true
    } else {
        current_hash
            .as_deref()
            .is_some_and(|hash| record.binary_hash == hash)
    };
    if mtime_ok && hash_ok {
        LockAction::Attach { pid }
    } else {
        LockAction::Superseded { pid }
    }
}

/// Atomic replace under the shared state lock: write a sibling temp file,
/// then rename it into place. The lock prevents a lease heartbeat and a
/// dispatcher write from losing each other's fields; the rename prevents a
/// reader from observing a half-written record.
pub fn write_state(path: &Path, state: &DoorbellState) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    let _lock = lock_state(path)?;
    let mut value = fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("state record is not an object: {}", path.display()))?;
    if let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(state) {
        object.extend(fields);
    }
    for (key, present) in [
        ("pid", state.pid.is_some()),
        ("project", state.project.is_some()),
        ("generation", state.generation.is_some()),
    ] {
        if !present {
            object.remove(key);
        }
    }
    let tmp = path.with_extension("state.tmp");
    fs::write(&tmp, serde_json::to_vec(&value)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

/// Exclusive advisory lock shared with `doorbell-rt`'s lease writer. The
/// critical section is tiny, so a bounded wait is enough to survive a
/// competing heartbeat without parking a dispatcher forever.
fn lock_state(path: &Path) -> Result<fs::File> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    const POLL: std::time::Duration = std::time::Duration::from_millis(5);
    let lock_path = path.with_extension("state.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open state lock {}", lock_path.display()))?;
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(POLL);
            }
            Err(fs::TryLockError::WouldBlock) => {
                anyhow::bail!("timed out locking state {}", lock_path.display());
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("lock state {}", lock_path.display()));
            }
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

#[cfg(test)]
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
#[cfg(test)]
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

#[cfg(test)]
pub fn inspect_lock_with(path: &Path, alive: impl Fn(u32) -> bool) -> LockAction {
    match read_pid(path) {
        None => LockAction::Start,
        Some(pid) if alive(pid) => LockAction::Attach { pid },
        Some(pid) => LockAction::Reloaded { stale: pid },
    }
}

/// Generation-aware lock decision (bd-stale-doorbell-wake-0fic). Attach
/// requires BOTH a live pid and a recorded generation matching
/// `current_identity`; anything alive otherwise is
/// [`LockAction::Superseded`]. Hash-aware: when the recorded gen carries a
/// hash, the on-disk binary hash must match too (same-second rebuild guard).
#[cfg_attr(not(test), allow(dead_code))]
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
                // No hash input on this path (cf. `inspect_lock_generation_with_hash`):
                // an empty hash means nothing to check, a carried-but-unverifiable
                // hash never attaches (fail-closed, same as `with_hash` with no
                // current hash). Empty-hash callers (pre-hash watchers) are unaffected.
                let hash_ok = record.binary_hash.is_empty();
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
    if let Some(pid) = dispatcher_pid(state_dir, workspace) {
        kill_pid(pid);
    }
    let _ = fs::remove_file(unanswered_file(state_dir, workspace));
    let state_path = state_file(state_dir, workspace);
    if state_path.exists() {
        let _lock = lock_state(&state_path);
        let _ = fs::remove_file(&state_path);
    }
    // Remove sidecars left by a pre-unification dispatcher. They are never
    // written or consulted by this build.
    let _ = fs::remove_file(pid_file(state_dir, workspace));
    let _ = fs::remove_file(gen_file(state_dir, workspace));
    let _ = fs::remove_file(project_sidecar(state_dir, workspace));
}

/// Drop a retired host's state record so `doorbell start`/`supervise` stop
/// re-arming it. `doorbell_project_workspaces` arms exactly the workspaces
/// that still carry a state file, so a host that confirmed `workspace-gone`
/// would otherwise be respawned every minute for the rest of the supervisor's
/// life. Only the record written by `pid` is removed: a replacement host that
/// already rewrote the record is never clobbered.
pub fn retire_workspace(state_dir: &Path, workspace: &str, pid: u32) {
    if read_state(state_dir, workspace).and_then(|s| s.pid) != Some(pid) {
        return;
    }
    let path = state_file(state_dir, workspace);
    let _lock = lock_state(&path);
    let _ = fs::remove_file(&path);
}

/// One-shot upgrade sweep for the pre-unification pid/gen/project sidecars.
///
/// A sidecar family is removed only when it cannot still belong to a live
/// legacy dispatcher: no pid, a dead pid, or no pid at all. A live pid or an
/// unreadable pid file is preserved (fail closed) so an upgrade never orphans
/// a process that is still running. Returns the number of files removed.
pub fn cleanup_legacy_sidecars(state_dir: &Path) -> usize {
    const SUFFIXES: [&str; 3] = [".pid", ".gen", ".project"];
    let dir = state_dir.join("doorbell");
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let mut workspaces = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        for suffix in SUFFIXES {
            if let Some(workspace) = name.strip_suffix(suffix)
                && !workspace.is_empty()
            {
                workspaces.insert(workspace.to_string());
            }
        }
    }

    let mut removed = 0usize;
    for workspace in workspaces {
        match fs::read_to_string(pid_file(state_dir, &workspace)) {
            Ok(raw) => match raw.trim().parse::<u32>() {
                Ok(pid) if pid_alive(pid) => continue,
                Ok(_) => {}
                Err(_) => continue,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => continue,
        }
        for suffix in SUFFIXES {
            let path = dir.join(format!("{workspace}{suffix}"));
            if fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// What the SELECTED plane says about the workspace a host watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacePresence {
    Present,
    Gone,
    /// The plane could not be read. NEVER an exit: unknown is not absent, the
    /// same rule the captain's stall probe applies to a degraded fleet. A
    /// plane mid-restart must not kill a live host.
    Unreadable,
}

/// Consecutive `Gone` probes needed before a host retires. One miss is a plane
/// mid-restart; three in a row (90s at the 30s probe cadence) is a workspace
/// that no longer exists. bd-gefp.
pub const WORKSPACE_GONE_STREAK: u32 = 3;

/// The exit decision, pure: `seen` is the running `Gone` count. `Present` and
/// `Unreadable` RESET it, so a transient plane hiccup can never retire a live
/// host.
pub fn host_exit_reason(seen: &mut u32, presence: WorkspacePresence) -> Option<&'static str> {
    match presence {
        WorkspacePresence::Gone => {
            *seen += 1;
            (*seen >= WORKSPACE_GONE_STREAK).then_some("workspace-gone")
        }
        WorkspacePresence::Present | WorkspacePresence::Unreadable => {
            *seen = 0;
            None
        }
    }
}

/// Journal ONE host-exit reason row. The audit channel, not the tick journal:
/// an exit is not a tick, and the audit log is the evidence channel for
/// lifecycle facts (`FileStore::append_audit` is its single writer).
pub fn journal_host_exit(state_dir: &Path, workspace: &str, reason: &str, streak: u32) {
    let store = doorbell_rt::FileStore::new(state_dir.to_path_buf());
    let entry = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "workspace": workspace,
        "kind": "host_exit",
        "reason": reason,
        "streak": streak,
    });
    let _ = store.append_audit(workspace, entry);
}

/// One liveness evaluation: decide, and journal WHY when the host retires.
/// The liveness poll and the witness test both call THIS, so neither the
/// decision nor the journal write can be deleted and stay green.
pub fn host_exit_check(
    state_dir: &Path,
    workspace: &str,
    seen: &mut u32,
    presence: WorkspacePresence,
) -> Option<&'static str> {
    let reason = host_exit_reason(seen, presence)?;
    journal_host_exit(state_dir, workspace, reason, *seen);
    Some(reason)
}

/// Pure classification of a workspace listing: an empty listing proves
/// nothing (plane mid-restart), so it is `Unreadable`, never `Gone`.
/// `Gone` needs a non-empty listing that lacks the workspace.
pub fn classify_list_presence(list_empty: bool, found: bool) -> WorkspacePresence {
    if found {
        WorkspacePresence::Present
    } else if list_empty {
        WorkspacePresence::Unreadable
    } else {
        WorkspacePresence::Gone
    }
}

/// Ask the selected plane whether this host's workspace still resolves.
/// A read error is `Unreadable`, never `Gone`.
pub fn probe_workspace_presence(
    cfg: &crate::config::Config,
    project: &str,
    workspace: &str,
) -> WorkspacePresence {
    let Ok(backend) = crate::backend::multiplexer_backend(cfg) else {
        return WorkspacePresence::Unreadable;
    };
    let Ok(list) = backend.workspace_list() else {
        return WorkspacePresence::Unreadable;
    };
    let mut labels =
        crate::workspace_label_aliases(project, crate::state::load_spawn_meta(project).as_deref());
    if !labels.iter().any(|l| l == workspace) {
        labels.push(workspace.to_string());
    }
    let found = list.iter().any(|ws| labels.iter().any(|l| l == &ws.label));
    classify_list_presence(list.is_empty(), found)
}

/// Cutover registration (bd-asr2.13, Phase 3): spawn records one
/// subscription row per pinned crew member (hc sink, own workspace —
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
            sink: SinkKind::Hc,
            budget: Budget::default_530(),
            project: Some(project.to_string()),
            endpoint: None,
        };
        if let Err(e) = store.subscribe(Actor::local_process(), sub, workspace) {
            eprintln!("flywheel: cannot record wake subscription for {pin}: {e:?}");
        }
    }
}

/// The arrival subscription scope: the project's hosting label, never the
/// arriving pane's own session. The one dispatcher for the project hosts this
/// row and the plane resolves the bound block across sessions; a row scoped to
/// a session no dispatcher hosts is silently deaf (bd-cf5kb).
pub fn arrival_scope(hosting_label: &str) -> String {
    hosting_label.to_string()
}

/// A dispatcher for the PROJECT, regardless of which workspace key its state
/// record uses. The live host may key its record by a session id while callers
/// read by label; a project match is the honest liveness answer.
pub fn dispatcher_token_for_project(state_dir: &Path, project: &str) -> &'static str {
    let dir = state_dir.join("doorbell");
    let Ok(entries) = fs::read_dir(&dir) else {
        return "absent";
    };
    let mut down = false;
    for entry in entries.flatten() {
        let Ok(raw) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<DoorbellState>(&raw) else {
            continue;
        };
        if state.project.as_deref() != Some(project) {
            continue;
        }
        if state.pid.is_some_and(pid_alive) {
            return "up";
        }
        if state.pid.is_some() {
            down = true;
        }
    }
    if down { "down" } else { "absent" }
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
        sink: SinkKind::Hc,
        budget: Budget::default_530(),
        project: Some(project.to_string()),
        endpoint: None,
    };
    if let Err(e) = store.subscribe(Actor::local_process(), sub, workspace) {
        eprintln!("flywheel: cannot record arrival wake subscription for {pin}: {e:?}");
    }
}

/// bd-cf5kb: the arrival row is scoped to the project's hosting label, never
/// the arriving pane's own session. The one dispatcher hosts it and the plane
/// resolves the bound block across sessions.
#[test]
fn arrival_scope_is_the_hosting_label() {
    assert_eq!(arrival_scope("flywheel-flywheel"), "flywheel-flywheel");
    assert_ne!(
        arrival_scope("flywheel-flywheel"),
        "adr-21-22-review",
        "a session-scoped row is hosted by nobody"
    );
}

/// bd-cf5kb: a dispatcher record keyed by any workspace/session still answers
/// for its project, so arrive does not report absent while one runs.
#[test]
fn dispatcher_token_for_project_scans_session_keyed_records() {
    let dir = std::env::temp_dir().join(format!("dbll-proj-token-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("doorbell")).unwrap();
    write_state(
        &state_file(&dir, "session:01a1d257"),
        &DoorbellState {
            pid: Some(std::process::id()),
            project: Some("flywheel".to_string()),
            generation: None,
        },
    )
    .unwrap();
    assert_eq!(
        dispatcher_token_for_project(&dir, "flywheel"),
        "up",
        "a session-keyed record for the project is a live dispatcher"
    );
    assert_eq!(dispatcher_token_for_project(&dir, "other"), "absent");
    let _ = std::fs::remove_dir_all(&dir);
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
    if let Err(e) = crate::append_pending_fact(state_dir, project, summary) {
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
    if let Err(e) = crate::append_pending_fact(state_dir, project, &summary) {
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

/// A refused wake waiting for a live dispatcher. Keyed by PROJECT: the CLI
/// cannot know the dispatcher's plane session id, and a workspace key is what
/// made the deferral invisible to the host. The host that owns the pin takes
/// it; every other dispatcher leaves it alone.
fn deferred_wakes_path(state_dir: &Path, project: &str) -> PathBuf {
    state_dir.join(format!(
        "doorbell-deferred-{}.jsonl",
        crate::state::state_key(project)
    ))
}

/// The line the dispatcher types for a deferred crew wake. It must be a mail
/// wake, never routine chatter: the fold drops a non-urgent, non-mail prompt
/// line (bd-346jl), so any other text would be silently swallowed at the
/// drain.
pub fn deferred_wake_line() -> String {
    doorbell_core::doorbell_line(doorbell_core::EventClass::PlanePMail, "toron next")
}

/// Hand one refused wake to the live dispatcher. The host folds it as
/// `PromptRefused`: a working pin queues it, and the next at-prompt status
/// rings it. Written under the journal's cross-process lock, and the file is
/// opened INSIDE the lock, so an append can never land between a drain's read
/// and its rewrite.
pub fn append_deferred_wake(
    state_dir: &Path,
    project: &str,
    pin: &str,
    line: &str,
) -> std::io::Result<()> {
    let path = deferred_wakes_path(state_dir, project);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut row = serde_json::json!({ "pin": pin, "line": line }).to_string();
    row.push('\n');
    let store = doorbell_rt::FileStore::new(state_dir);
    store.with_file_lock(&path, |_| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        f.write_all(row.as_bytes())?;
        f.flush()
    })?
}

/// Take the wakes this host hosts, leaving the rest for the dispatcher that
/// owns those pins (a project can run one dispatcher per plane session, and a
/// host that takes a foreign pin's wake queues it for a pane it never
/// prompts). Each taken wake is journaled BEFORE the handoff is rewritten, so
/// a crash cannot drop it: the row is the durable record, and an unrewritten
/// file simply re-drains (at-least-once).
pub fn take_deferred_wakes(
    state_dir: &Path,
    project: &str,
    workspace: &str,
    hosted: &std::collections::BTreeSet<String>,
) -> Vec<(String, String)> {
    let path = deferred_wakes_path(state_dir, project);
    let store = doorbell_rt::FileStore::new(state_dir);
    let taken = store.with_file_lock(&path, |_| -> std::io::Result<Vec<(String, String)>> {
        let Ok(raw) = fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        let mut mine: Vec<(String, String)> = Vec::new();
        let mut rest = String::new();
        for row in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(row.trim()) else {
                continue;
            };
            let entry = match (
                v.get("pin").and_then(|p| p.as_str()),
                v.get("line").and_then(|l| l.as_str()),
            ) {
                (Some(pin), Some(line)) => (pin.to_string(), line.to_string()),
                _ => continue,
            };
            if hosted.contains(&entry.0) {
                mine.push(entry);
            } else {
                rest.push_str(&v.to_string());
                rest.push('\n');
            }
        }
        if mine.is_empty() {
            return Ok(Vec::new());
        }
        for (pin, line) in &mine {
            let msg = crate::workspace::Msg::PromptRefused {
                pin: pin.clone(),
                line: line.clone(),
            };
            store
                .append_journal(
                    workspace,
                    &serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null),
                    &serde_json::Value::Array(Vec::new()),
                    serde_json::json!({ "stage": "handoff", "pin": pin }),
                    std::slice::from_ref(pin),
                )
                .map_err(|e| std::io::Error::other(format!("journal the deferred wake: {e}")))?;
        }
        if rest.is_empty() {
            let _ = fs::remove_file(&path);
        } else {
            let tmp = path.with_extension("rewrite");
            fs::write(&tmp, rest)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(mine)
    });
    match taken {
        Ok(Ok(entries)) => entries,
        Ok(Err(e)) | Err(e) => {
            eprintln!("flywheel: deferred wake handoff not drained (retried next sweep): {e}");
            Vec::new()
        }
    }
}

pub fn inject_wake_in(
    state_dir: &Path,
    project: &str,
    workspace: &str,
    pin: &str,
    line: &str,
    now_unix: u64,
) -> Result<String, InjectDeny> {
    let journal_path = doorbell_rt::FileStore::new(state_dir).journal_file(workspace);
    let journal_tail =
        crate::state::read_tail_lines(&journal_path, 32, 256 * 1024).unwrap_or_default();
    {
        for row in journal_tail.iter().rev().take(32) {
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
    let budget_path = inject_budget_path(state_dir, project);
    let summary = format!("inject {pin}: {line}");
    let msg = crate::workspace::Msg::Fact {
        summary: summary.clone(),
    };
    let mut denied = false;
    crate::state::update_locked(
        &budget_path,
        |budgets: &mut std::collections::BTreeMap<String, Vec<u64>>| {
            let stamps = budgets.entry(pin.to_string()).or_default();
            stamps.retain(|t| now_unix.saturating_sub(*t) < 1800);
            if stamps.len() >= 3 {
                denied = true;
                return;
            }
            journal_append_workspace(
                state_dir,
                workspace,
                &msg,
                &[],
                serde_json::json!({
                    "stage": "inject",
                    "pin": pin,
                    "workspace": workspace,
                    "project": project,
                    "inject_ts": now_unix,
                }),
                Some(&[pin.to_string()]),
            );
            stamps.push(now_unix);
        },
    )
    .map_err(|_| InjectDeny::Budget)?;
    if denied {
        return Err(InjectDeny::Budget);
    }
    let _ = crate::append_pending_fact(state_dir, project, &summary);
    Ok(summary)
}

/// Wait on a detached child so the OS can reap it. Dropping a `Child` does not
/// reap it on Unix; a long-lived supervisor would otherwise keep one
/// `<defunct>` entry per respawned host until the supervisor itself exits.
fn reap_detached(workspace: &str, mut child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name(format!("doorbell-reap-{workspace}"))
        .spawn(move || {
            let _ = child.wait();
        });
}

fn spawn_child(exe: &Path, state_dir: &Path, project: &str, workspace: &str) -> Result<()> {
    let state_path = state_file(state_dir, workspace);
    let project = canonical_project(project);
    let mut cmd = Command::new(exe);
    cmd.args([
        "__doorbell",
        "--project",
        &project,
        "--workspace",
        workspace,
    ]);
    cmd.stdin(Stdio::null());
    // Keep the host's own diagnostics. `Stdio::null()` made a wedged or
    // crash-looping host invisible (the reason had to be reconstructed from
    // journals). One append log per workspace, beside the state record.
    let log_dir = state_path.parent().map(std::path::Path::to_path_buf);
    let mut attached = false;
    if let Some(dir) = log_dir
        && std::fs::create_dir_all(&dir).is_ok()
        && let (Ok(out), Ok(err)) = (
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{workspace}.out.log"))),
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{workspace}.err.log"))),
        )
    {
        cmd.stdout(Stdio::from(out));
        cmd.stderr(Stdio::from(err));
        attached = true;
    }
    if !attached {
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn doorbell for {workspace}"))?;
    let pid = child.id();
    reap_detached(workspace, child);
    let generation = WatcherGen::capture(exe, unix_now());
    write_state(
        &state_path,
        &DoorbellState {
            pid: Some(pid),
            project: Some(project.to_string()),
            generation,
        },
    )?;
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
    let project = &canonical_project(project);
    let _ = cleanup_legacy_sidecars(state_dir);
    let loaded = crate::config::Config::load().ok();
    let comms_on = match &loaded {
        Some(c) => crate::comms_send::enabled(c),
        None => toron_core::inbox::try_enabled_with(None).unwrap_or(true),
    };
    if comms_on {
        eprintln!(
            "flywheel doorbell: comms flag on — the local loop hosts the inbox wake (no deference to a daemon)"
        );
    }
    let cfg_url = loaded.and_then(|c| c.doorbell_url());
    if !comms_on
        && cfg_url.is_some()
        && let crate::daemon::DoorbellEnsure::Degraded { hint } =
            crate::daemon::ensure_doorbell_spawn()
    {
        eprintln!("flywheel doorbell: {hint}");
    }
    if !comms_on
        && matches!(
            resolve_host(state_dir, cfg_url, Some((project, workspace))),
            DoorbellHost::Proxy { .. }
        )
    {
        eprintln!(
            "flywheel doorbell: daemon host alive — attached, local loop suppressed (deference)"
        );
        return Ok(LockAction::Attach { pid: 0 });
    }
    let state = read_state(state_dir, workspace);
    let action = inspect_state_generation(
        state.as_ref(),
        binary_identity(exe),
        binary_hash(exe),
        pid_alive,
    );
    match action {
        LockAction::Attach { .. } => Ok(action),
        LockAction::Superseded { pid } => {
            kill_pid(pid);
            let path = state_file(state_dir, workspace);
            if let Ok(_lock) = lock_state(&path) {
                let _ = fs::remove_file(&path);
            }
            spawn_child(exe, state_dir, project, workspace)?;
            Ok(action)
        }
        LockAction::Start | LockAction::Reloaded { .. } => {
            if matches!(action, LockAction::Reloaded { .. }) {
                let path = state_file(state_dir, workspace);
                if let Ok(_lock) = lock_state(&path) {
                    let _ = fs::remove_file(&path);
                }
            }
            spawn_child(exe, state_dir, project, workspace)?;
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
/// Which host runs the loop for `flywheel __doorbell`. With the comms flag on
/// the pane writer is the inbox host inside THIS process; a daemon host types
/// nothing, so "a healthy daemon hosts this workspace" must not defer the
/// loop to it (it left the workspace with no ringer: flywheel#22 gap S3).
/// Flag off: the resolver decides, as before.
/// Daemon `start` is the whole start only while that daemon still types.
/// Comms on, it types nothing, so the local loop (inbox writer and the
/// timer sweep) still arms beside it.
pub fn local_loop_after_proxy(proxied: bool, comms_on: bool) -> bool {
    !proxied || comms_on
}

pub fn host_for(comms_on: bool, resolve: impl FnOnce() -> DoorbellHost) -> DoorbellHost {
    if comms_on {
        DoorbellHost::Embedded
    } else {
        resolve()
    }
}

pub fn resolve_host(
    state_dir: &Path,
    cfg_url: Option<String>,
    target: Option<(&str, &str)>,
) -> DoorbellHost {
    use doorbell_rt::lease::{deference_decision, live_leases};
    const DEFAULT_URL: &str = "http://127.0.0.1:18980";
    let now = doorbell_rt::lease::unix_now();
    let probe = target.and_then(|(_, ws)| {
        live_leases(state_dir, now)
            .into_iter()
            .find(|(w, _)| w == ws)
            .map(|(_, lease)| lease)
    });
    let resolve_at = |url: &str, configured: bool| -> DoorbellHost {
        let (health, health_err) = match daemon_health_probe(url) {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let hosts_target = match (&health, target) {
            (Some(v), Some((p, w))) => daemon_hosts(v, p, w),
            (Some(_), None) => true,
            (None, _) => false,
        };
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
                    let cause = health_err.map_or("unknown", |e| e.reason());
                    let remediation = match health_err {
                        Some(HealthProbeFailure::NotListening) | None => {
                            "remediation: doorbell start | doorbell install | flywheel daemon start"
                        }
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

/// Sidecar: which project owns a workspace dispatcher (written by
/// `spawn_child`; the pid/gen files alone don't carry it).
pub fn project_sidecar(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("{workspace}.project"))
}

pub fn read_project_sidecar(state_dir: &Path, workspace: &str) -> Option<String> {
    dispatcher_project(state_dir, workspace)
}

/// Every dispatcher workspace whose pid is missing or dead: (label, project).
/// Workspaces without a sidecar are skipped — they predate the supervisor.
pub fn dead_dispatchers(state_dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let Ok(entries) = fs::read_dir(state_dir.join("doorbell")) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or_default();
        let Some(label) = name.strip_suffix(".state.json").map(str::to_string) else {
            continue;
        };
        if label.is_empty() || !seen.insert(label.clone()) {
            continue;
        }
        let state = read_state(state_dir, &label);
        let alive = state.as_ref().and_then(|s| s.pid).is_some_and(pid_alive);
        if !alive && let Some(project) = state.and_then(|s| s.project) {
            out.push((label, project));
        }
    }
    out
}

/// The heal itself as a plain async fn (bd-flywheel-cron-elm-cn7p): the
/// ElmCron host calls this directly; the old durable leaf and macro are gone.
pub async fn heal_tick_body() -> anyhow::Result<Vec<String>> {
    let state_dir = crate::config::state_dir();
    let exe = std::env::current_exe()?;
    let mut healed = Vec::new();
    for (label, project) in dead_dispatchers(&state_dir) {
        if let Err(e) = ensure_running(&exe, &state_dir, &label, &project) {
            return Err(anyhow::anyhow!("supervisor heal {label}: {e:#}"));
        }
        healed.push(label.clone());
    }
    Ok(healed)
}

/// The cron the ElmCron host fires on: every minute (bd-flywheel-cron-elm-cn7p:
/// folded from the server-side supervisor registration, which is gone).
pub const SUPERVISOR_CRON: &str = "* * * * *";
pub const SUPERVISOR_NAME: &str = "doorbell-supervisor";

pub fn report_text(state_dir: &Path, workspace: &str) -> String {
    let head = dispatcher_status_line(state_dir, workspace);
    let gen_tail = match dispatcher_generation(state_dir, workspace) {
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
    doorbell_rt::file_host::rotate_audit_if_oversized(&path);
}

/// Idle horizon for [`dead_audit_files`] when no explicit one is given.
pub const AUDIT_PRUNE_DAYS: u64 = 30;

/// One audit file `doorbell gc` would delete, and why it counts as dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadAuditFile {
    pub workspace: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// `project-gone` or `idle`; never a guess.
    pub reason: &'static str,
}

/// Audit logs owned by workspaces no dispatcher will serve again.
///
/// Rotation bounds a LIVE file, but nothing here is ever removed: one
/// `.audit.jsonl` per workspace that ever ran a dispatcher, including the
/// throwaway temp roots of finished gate runs. This finds those, and only
/// those. A workspace is dead when no live dispatcher holds its pid AND
///
/// - its unified state record names an absolute project path that no longer
///   exists (a temp root from a finished gate run), or
/// - the file has not been written for `horizon_days`.
///
/// A bare project name (`flywheel`), which is how `doorbell start` records a
/// human project, cannot be tested for existence, so it only ever prunes on
/// age. Deleting a live diagnostic is worse than keeping a few MB.
pub fn dead_audit_files(
    state_dir: &Path,
    horizon_days: u64,
    now: std::time::SystemTime,
    alive: impl Fn(u32) -> bool,
) -> Vec<DeadAuditFile> {
    let dir = state_dir.join("doorbell");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(workspace) = name.strip_suffix(".audit.jsonl") else {
            continue;
        };
        let path = entry.path();
        if dispatcher_pid(state_dir, workspace).is_some_and(&alive) {
            continue;
        }
        let Ok(md) = entry.metadata() else {
            continue;
        };
        let Some(mtime) = md
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
        else {
            continue;
        };
        let now_secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let project_gone = dispatcher_project(state_dir, workspace)
            .is_some_and(|p| Path::new(&p).is_absolute() && !Path::new(&p).exists());
        let Some(abandon) = doorbell_rt::store::audit_abandon_reason(
            workspace,
            mtime,
            project_gone,
            &std::collections::BTreeSet::new(),
            horizon_days.saturating_mul(86_400),
            now_secs,
        ) else {
            continue;
        };
        out.push(DeadAuditFile {
            workspace: workspace.to_string(),
            path,
            bytes: md.len(),
            reason: match abandon {
                doorbell_rt::store::AuditAbandon::ProjectGone => "project-gone",
                doorbell_rt::store::AuditAbandon::Idle => "idle",
            },
        });
    }
    out.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    out
}

/// Tests below port the same scenarios to `workspace::tick` (see `sc_d1_*`).
/// hc-mock: stdin watch lines + pin statuses → JSON injects via tick (no second reducer).
pub fn once_json(stdin: &str, status_by_pin: &BTreeMap<String, String>) -> String {
    let index = SelfIndex::from_pins(status_by_pin.keys().cloned());
    let mut model = crate::workspace::WorkspaceModel::default();
    model.doorbell.index = index;
    for (pin, status) in status_by_pin {
        let (next, _) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::PlaneStatus {
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

/// The workspace labels one project's doorbell state lives under.
///
/// Two labels mean the same project: the project-scoped one
/// (`flywheel-{project}`, which is also what the unanswered file is named),
/// and any dispatcher whose `.project` sidecar names the project under a
/// label of its own (`adrx2728` for `bd-…`). Both are honoured because a
/// digest asked about one project must never answer with another's wakes
/// (`elm-effect-rs-doorbell-digest-project-scope-x3s2`), and picking only the
/// canonical spelling would go silent on exactly the dispatchers the sidecar
/// exists to record.
///
/// A workspace that has an unanswered file but no journal yet is claimed by
/// the canonical label: that file is created before the first wake, and it is
/// the one piece of state the digest escalates.
pub fn project_workspaces(state_dir: &Path, project: &str) -> std::collections::BTreeSet<String> {
    let canonical = format!("flywheel-{project}");
    let mut out: std::collections::BTreeSet<String> = journal_workspaces_list(state_dir)
        .into_iter()
        .filter(|ws| {
            ws == &canonical || read_project_sidecar(state_dir, ws).as_deref() == Some(project)
        })
        .collect();
    if unanswered_file(state_dir, &canonical).exists() {
        out.insert(canonical);
    }
    out
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

/// Nearby discovery row (bd-gpj0.2): hc terminals ∪ registry pins ∪
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

/// Pure join: registry `(pin, pane, name)`, hc `(pane, name, status)`,
/// leases `(holder, path)`. Unbound panes stay in the list.
pub fn join_nearby(
    registry: &[(String, String, String)],
    hc: &[(String, String, String)],
    leases: &[(String, String)],
) -> Vec<NearbyRow> {
    let mut status_by_pane: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    for (pane, name, status) in hc {
        status_by_pane.insert(pane.clone(), (name.clone(), status.clone()));
    }
    let mut seen_panes = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    for (pin, pane, name) in registry {
        seen_panes.insert(pane.clone());
        let (hname, status) = status_by_pane
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
    for (pane, (name, status)) in &status_by_pane {
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
    /// bd-0gup: the inject budget must be charged under ONE lock.
    ///
    /// The old code did an unlocked load -> check -> `std::fs::write`
    /// (truncating in place): every concurrent injector could read the same
    /// budget, all pass `len >= 3`, and the file could be left truncated (a
    /// crash mid-write reset the budget to empty). The charge now happens
    /// under the shared state lock with an atomic write, so the budget holds
    /// under concurrency.
    #[test]
    fn inject_budget_lock_serializes_the_charge() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = std::env::temp_dir().join(format!("fw-inject-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ok = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..8 {
            let dir = dir.clone();
            let ok = ok.clone();
            handles.push(std::thread::spawn(move || {
                if super::inject_wake_in(
                    &dir,
                    "proj",
                    "ws",
                    "AmberOwl",
                    &format!("line-{i}"),
                    1_000_000,
                )
                .is_ok()
                {
                    ok.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }
        let landed = ok.load(Ordering::SeqCst);
        assert!(
            landed <= 3,
            "the budget of 3 injects per window must hold under concurrency: {landed}"
        );
        assert!(landed >= 1, "at least one inject must land: {landed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Follow-up (a): a daemon host that hosts the workspace but types nothing
    /// (comms flag) must not take the loop from `flywheel __doorbell`.
    #[test]
    fn under_the_comms_flag_a_daemon_that_hosts_the_workspace_does_not_take_the_loop() {
        use super::{DoorbellHost, host_for};
        let daemon = || DoorbellHost::Proxy {
            url: "http://127.0.0.1:18981".into(),
        };
        assert!(matches!(host_for(true, daemon), DoorbellHost::Embedded));
        assert!(
            matches!(host_for(false, daemon), DoorbellHost::Proxy { .. }),
            "flag off: deference to a healthy daemon host, unchanged"
        );
        let mut asked = false;
        let _ = host_for(true, || {
            asked = true;
            DoorbellHost::Embedded
        });
        assert!(!asked);
        let cmd = include_str!("cmd/doorbell.rs");
        assert!(cmd.contains("crate::doorbell::host_for(comms_on"));
    }

    /// A proxied daemon start must still arm the local loop when comms is on.
    /// Flag off, the proxy is the whole start.
    #[test]
    fn a_proxied_start_still_arms_the_local_loop_when_comms_is_on() {
        use super::local_loop_after_proxy;
        assert!(!local_loop_after_proxy(true, false));
        assert!(local_loop_after_proxy(true, true));
        assert!(local_loop_after_proxy(false, false));
        assert!(local_loop_after_proxy(false, true));
        let cmd = include_str!("cmd/doorbell.rs");
        let start = cmd.find("pub(crate) fn cmd_doorbell_start(").unwrap();
        let body = &cmd[start..start + 1800];
        assert!(
            body.contains("local_loop_after_proxy(proxied"),
            "start must use the gate"
        );
    }

    /// bd-doorbell-daemon-sink-wake-only-yfqz: under the comms flag the local
    /// loop hosts the inbox wake, so it must not defer to a daemon host.
    #[test]
    fn the_comms_flag_disables_deference_to_a_daemon() {
        let src = include_str!("doorbell.rs");
        let a = src.find("let comms_on =").unwrap();
        let tail = &src[a..a + 1800];
        assert!(tail.contains("if !comms_on\n        && cfg_url.is_some()"));
        assert!(tail.contains("if !comms_on\n        && matches!(\n            resolve_host("));
    }

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

    /// An injected row must be attributable to its pin, or
    /// `doorbell why <pin>` cannot explain the operator console door.
    /// The pin lives in the envelope `pins` field because a `Fact` msg
    /// carries no pin of its own.
    #[test]
    fn injected_row_is_attributable_to_its_pin() {
        let dir = std::env::temp_dir().join(format!("inject-attr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let summary = inject_wake_in(&dir, "proj", "ws", "CrimsonHeron", "probe", 1_000)
            .expect("first inject is allowed");
        assert!(summary.contains("CrimsonHeron"));

        let entries = journal_read_all(&dir);
        let entry = entries
            .iter()
            .find(|(_, v)| {
                v.get("msg")
                    .and_then(|m| m.get("Fact"))
                    .and_then(|f| f.get("summary"))
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s.contains("probe"))
            })
            .map(|(_, v)| v)
            .expect("the injected row is journaled");
        let pins: Vec<&str> = entry
            .get("pins")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            pins.contains(&"CrimsonHeron"),
            "the injected row must carry its pin in the envelope attribution: {entry}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The handoff is keyed by PROJECT (the CLI knows no dispatcher session),
    /// and a host takes only the pins it hosts: with one dispatcher per plane
    /// session, a foreign pin's wake must survive for the host that can ring
    /// it. The journal row is written before the file is rewritten, so a
    /// crash re-drains instead of dropping.
    #[test]
    fn a_deferred_wake_goes_to_the_host_that_owns_the_pin() {
        let dir = std::env::temp_dir().join(format!("fw-defer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let line = deferred_wake_line();
        append_deferred_wake(&dir, "flywheel", "BlueRidge", &line).expect("record");
        append_deferred_wake(&dir, "flywheel", "LilacChapel", &line).expect("record");
        let hosted: std::collections::BTreeSet<String> =
            ["BlueRidge".to_string()].into_iter().collect();

        assert!(
            take_deferred_wakes(&dir, "other-project", "session:a", &hosted).is_empty(),
            "another project never drains this project's wakes"
        );
        assert_eq!(
            take_deferred_wakes(&dir, "flywheel", "session:a", &hosted),
            vec![("BlueRidge".to_string(), line.clone())],
            "the host takes its own pin's wake"
        );
        assert_eq!(
            take_deferred_wakes(&dir, "flywheel", "session:b", &hosted),
            Vec::new(),
            "a taken wake is not re-rung"
        );
        let left = take_deferred_wakes(
            &dir,
            "flywheel",
            "session:b",
            &["LilacChapel".to_string()].into_iter().collect(),
        );
        assert_eq!(
            left,
            vec![("LilacChapel".to_string(), line.clone())],
            "the foreign pin's wake survived for the host that owns it"
        );
        assert!(
            doorbell_core::is_mail_wake(&line),
            "the deferred line survives the routine-chatter drop: {line}"
        );
        let journal = std::fs::read_to_string(
            dir.join("journals")
                .join("doorbell-journal-session:a.jsonl"),
        )
        .expect("the taken wake is journaled before the file is rewritten");
        assert!(
            journal.contains("\"PromptRefused\"") && journal.contains("BlueRidge"),
            "journal row before removal: {journal}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Appends and the drain share one cross-process lock, and the appender
    /// opens the file INSIDE it: a writer that opened before a drain's rewrite
    /// would append into a moved inode and be read by nobody. Every line must
    /// end up either taken (journaled) or still pending.
    #[test]
    fn concurrent_appends_survive_a_drain() {
        let dir = std::env::temp_dir().join(format!("fw-defer-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let line = deferred_wake_line();
        let total = 160usize;
        let hosted: std::collections::BTreeSet<String> =
            ["BlueRidge".to_string()].into_iter().collect();
        let mut taken = 0usize;
        std::thread::scope(|scope| {
            for t in 0..8 {
                let dir = dir.clone();
                let line = line.clone();
                scope.spawn(move || {
                    for i in 0..total / 8 {
                        let pin = format!("BlueRidge");
                        let _ = (t, i);
                        append_deferred_wake(&dir, "flywheel", &pin, &line).expect("append");
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..40 {
                    taken += take_deferred_wakes(&dir, "flywheel", "session:race", &hosted).len();
                }
            });
        });
        let left = take_deferred_wakes(&dir, "flywheel", "session:race", &hosted).len();
        assert_eq!(
            taken + left,
            total,
            "no append may be lost between a read and a rewrite (taken {taken}, left {left})"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        let json = once_json(kind9_at_blueridge(), &status);
        assert_eq!(
            json, "[]",
            "hc-only idle must not inject without presence authority: {json}"
        );
    }

    #[test]
    fn once_json_seeds_statuses_via_plane_tick() {
        let src = include_str!("doorbell.rs");
        let needle = ["model.doorbell.statuses ", "= status_by_pin"].concat();
        assert!(
            !src.contains(&needle),
            "RED: once_json still map-assigns statuses; tick Msg::PlaneStatus instead"
        );
        let mut working = BTreeMap::new();
        working.insert("BlueRidge".into(), "working".into());
        let json = once_json(kind9_at_blueridge(), &working);
        assert_eq!(json, "[]", "working pin must not inject: {json}");
        let mut idle = BTreeMap::new();
        idle.insert("BlueRidge".into(), "idle".into());
        let json = once_json(kind9_at_blueridge(), &idle);
        assert_eq!(
            json, "[]",
            "hc-only idle must not inject without presence authority: {json}"
        );
    }

    #[test]
    fn s_proof_why_producer_journal_path() {
        let dir = std::env::temp_dir().join(format!("fw-sproof-path-{}", std::process::id()));
        let path = journal_file(&dir, "ws1");
        assert_eq!(
            path,
            dir.join("journals").join("doorbell-journal-ws1.jsonl"),
            "journal path contract violated: {path:?}"
        );
    }

    /// x3s2: a project's digest scope is its own workspaces, never the host's.
    ///
    /// Three workspaces are present: the project's canonical label, a
    /// dispatcher carrying the project in its sidecar under a label of its
    /// own, and a third workspace belonging to nobody asked about. The scope
    /// must hold the first two and refuse the third, because the digest's
    /// whole job is answering "what is queued for THIS project".
    #[test]
    fn project_workspaces_scopes_to_the_named_project() {
        let dir = std::env::temp_dir().join(format!("fw-x3s2-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("journals")).unwrap();
        std::fs::create_dir_all(dir.join("doorbell")).unwrap();
        for ws in ["flywheel-beads", "flywheel-other", "adrx1", "session:test"] {
            std::fs::write(
                dir.join("journals")
                    .join(format!("doorbell-journal-{ws}.jsonl")),
                "",
            )
            .unwrap();
        }
        write_state(
            &state_file(&dir, "adrx1"),
            &DoorbellState {
                project: Some("beads".into()),
                ..Default::default()
            },
        )
        .unwrap();
        write_state(
            &state_file(&dir, "session:test"),
            &DoorbellState {
                project: Some("beads".into()),
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(
            dir.join("doorbell")
                .join("flywheel-elm-effect-rs.unanswered"),
            "SomePin\n",
        )
        .unwrap();

        let beads = project_workspaces(&dir, "beads");
        assert!(beads.contains("flywheel-beads"), "{beads:?}");
        assert!(beads.contains("adrx1"), "sidecar mapping lost: {beads:?}");
        assert!(
            beads.contains("session:test"),
            "a session host's unified project record must stay in scope: {beads:?}"
        );
        assert!(
            !beads.contains("flywheel-other"),
            "another project's workspace leaked into the scope: {beads:?}"
        );

        let effect = project_workspaces(&dir, "elm-effect-rs");
        assert_eq!(
            effect.into_iter().collect::<Vec<_>>(),
            vec!["flywheel-elm-effect-rs".to_string()],
            "the canonical label must be claimed from its unanswered file alone"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn s_proof_why_fold_reproduces_cmds() {
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
        let dir = std::env::temp_dir().join(format!("fw-sproof-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pins = vec!["BlueRidge".to_string()];
        let mut model = crate::workspace::WorkspaceModel::default();
        for p in &pins {
            model.doorbell.index.insert_pin(p.clone(), None);
        }
        let idle = crate::workspace::Msg::PlaneStatus {
            pin: "BlueRidge".into(),
            status: "idle".into(),
        };
        let (seeded, _) = crate::workspace::tick(&model, &idle);
        model = seeded;
        let presence = crate::workspace::Msg::Presence {
            pin: "BlueRidge".into(),
            state: "idle".into(),
        };
        let (seeded, _) = crate::workspace::tick(&model, &presence);
        model = seeded;
        let msg = crate::workspace::Msg::WatchNdjson {
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
        assert_eq!(cmds, [] as [crate::workspace::Cmd; 0]);
        journal_append_workspace(
            &dir,
            "ws1",
            &presence,
            &cmds,
            serde_json::json!({}),
            Some(&pins),
        );
        model = next;
        let idle = Msg::PlaneStatus {
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
        let (drained, cmds) = crate::workspace::tick(&folded, &Msg::Tick { now: 1_000_000 });
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::Digest { .. })),
            "digest batch emits: {cmds:?}"
        );
        assert_eq!(
            drained.wake.digests,
            [] as [doorbell_core::wake_core::DigestEntry; 0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "bd-l72k LIVE probe: needs live toron daemon + flywheel project + TurquoiseSparrow pin"]
    fn edge_triage_live_probe_urgent_only_injects() {
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
        let mut next = next;
        next.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        let (next2, cmds2) = crate::workspace::tick(
            &next,
            &crate::workspace::Msg::PlaneStatus {
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
        model.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
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
        assert!(hint.contains("toron next"));
    }

    #[test]
    fn sc_d2_routine_inbox_does_not_ring() {
        let raw = r#"{"importance":"normal","ack_only":false}"#;
        assert!(parse_inbox_item(&serde_json::from_str(raw).unwrap()).is_none());
    }

    #[test]
    fn plane_p_urgent_injects_when_idle() {
        use crate::workspace::{Msg, WorkspaceModel};
        let mut model = WorkspaceModel::default();
        model.doorbell.index.insert_pin("BlueRidge".into(), None);
        model.presence.insert(
            "BlueRidge".into(),
            crate::workspace::Presence::from_str("idle"),
        );
        let (model, _) = crate::workspace::tick(
            &model,
            &Msg::PlaneStatus {
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
        assert!(prompts[0].1.contains("toron next"));
        let raw = r#"{"messages":[{"importance":"urgent","created_ts":"2026-08-20T00:00:02Z"}]}"#;
        assert_eq!(
            inbox_high_water(raw).as_deref(),
            Some("2026-08-20T00:00:02Z")
        );
    }

    #[test]
    fn inbox_high_water_reads_created_ts_us_from_live_json() {
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
        let out = take_allowed(
            vec![("BlueRidge".into(), "[doorbell] a".into())],
            &mut budgets,
            0,
        );
        assert_eq!(out.len(), 1);
        let out = take_allowed(
            vec![("BlueRidge".into(), "[doorbell] b".into())],
            &mut budgets,
            10,
        );
        assert!(out.is_empty(), "still within 1m window");
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
        write_state(
            &state_file(&dir, "w1"),
            &DoorbellState {
                pid: Some(std::process::id()),
                ..Default::default()
            },
        )
        .expect("pid");
        let line = dispatcher_status_line(&dir, "w1");
        assert!(line.starts_with("dispatcher: up"), "{line}");
        let _ = std::fs::remove_file(state_file(&dir, "w1"));
        assert_eq!(dispatcher_status_line(&dir, "w1"), "dispatcher: down");
        write_state(
            &state_file(&dir, "w1"),
            &DoorbellState {
                pid: Some(999_998),
                ..Default::default()
            },
        )
        .expect("stale");
        stop(&dir, "w1");
        assert!(!state_file(&dir, "w1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatcher_token_absent_down_up() {
        let dir = std::env::temp_dir().join(format!("fw-doorbell-token-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("doorbell"));
        assert_eq!(dispatcher_token(&dir, "w1"), "absent");
        write_state(
            &state_file(&dir, "w1"),
            &DoorbellState {
                pid: Some(999_998),
                ..Default::default()
            },
        )
        .expect("stale");
        assert_eq!(dispatcher_token(&dir, "w1"), "down");
        write_state(
            &state_file(&dir, "w1"),
            &DoorbellState {
                pid: Some(std::process::id()),
                ..Default::default()
            },
        )
        .expect("live");
        assert_eq!(dispatcher_token(&dir, "w1"), "up");
        let _ = std::fs::remove_file(state_file(&dir, "w1"));
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
        assert_eq!(h.unanswered_pins(), [] as [String; 0]);
        assert_eq!(h.expire(200, UNANSWERED_SECS), [] as [String; 0]);
    }

    #[test]
    fn hear_timeout_marks_unanswered() {
        let mut h = HearTracker::default();
        h.note_inject("PaleHarbor", None, 10);
        assert_eq!(
            h.expire(10 + UNANSWERED_SECS - 1, UNANSWERED_SECS),
            [] as [String; 0]
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
        assert_eq!(h.unanswered_pins(), [] as [String; 0]);
    }

    #[test]
    fn hear_npub_filled_late_still_matches() {
        let mut h = HearTracker::default();
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        h.note_inject("PaleHarbor", None, 10);
        h.remember_npub("PaleHarbor", Some(npub));
        h.note_heard_pubkey(npub);
        assert_eq!(
            h.expire(10 + UNANSWERED_SECS, UNANSWERED_SECS),
            [] as [String; 0]
        );
        assert_eq!(h.unanswered_pins(), [] as [String; 0]);
    }

    #[test]
    fn hear_late_post_clears_unanswered() {
        let mut h = HearTracker::default();
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        h.note_inject("PaleHarbor", Some(npub), 0);
        let _ = h.expire(UNANSWERED_SECS, UNANSWERED_SECS);
        assert_eq!(h.unanswered_pins(), vec!["PaleHarbor".to_string()]);
        h.note_heard_pubkey(npub);
        assert_eq!(h.unanswered_pins(), [] as [String; 0]);
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
        assert_eq!(
            read_unanswered(&unanswered_file(&dir, "w1")),
            [] as [String; 0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_pubkey_from_live_shape() {
        let line = r#"{"kind":9,"id":"ab","pubkey":"AABBCC","tags":[],"content":"hi"}"#;
        assert_eq!(watch_pubkey(line).as_deref(), Some("aabbcc"));
    }

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
    fn state_write_preserves_lease_and_respawn_drops_absent_generation() {
        let dir = scratch("state-lease");
        let path = state_file(&dir, "w1");
        let record = WatcherGen {
            started_unix: 10,
            binary_mtime_unix: 5,
            binary_len: 77,
            binary_hash: String::new(),
        };
        write_state(
            &path,
            &DoorbellState {
                pid: Some(999_997),
                project: Some("proj".into()),
                generation: Some(record),
            },
        )
        .expect("initial state");
        let lease = doorbell_rt::lease::acquire_lease(&dir, "w1", 100).expect("lease");
        write_state(
            &path,
            &DoorbellState {
                pid: Some(999_998),
                project: Some("proj".into()),
                generation: None,
            },
        )
        .expect("respawn state");
        let state = read_state(&dir, "w1").expect("state");
        assert_eq!(state.pid, Some(999_998), "respawn replaces the pid");
        assert!(state.generation.is_none(), "absent generation is removed");
        assert_eq!(
            doorbell_rt::lease::read_lease(&dir, "w1"),
            Some(lease),
            "a dispatcher write keeps the owner lease"
        );
        stop(&dir, "w1");
        assert!(read_state(&dir, "w1").is_none());
        assert!(doorbell_rt::lease::read_lease(&dir, "w1").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn respawn_cycle_replaces_pid_and_generation() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("respawn");
        let exe = dir.join("fake-doorbell");
        fs::write(&exe, "#!/bin/sh\nexec sleep 30\n").expect("fake exe");
        let mut permissions = fs::metadata(&exe).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&exe, permissions).expect("chmod");

        spawn_child(&exe, &dir, "proj", "w1").expect("first spawn");
        let first = read_state(&dir, "w1").expect("first state");
        assert_eq!(first.project.as_deref(), Some("proj"));
        assert!(first.pid.is_some_and(pid_alive));
        stop(&dir, "w1");
        assert!(read_state(&dir, "w1").is_none());

        spawn_child(&exe, &dir, "proj", "w1").expect("second spawn");
        let second = read_state(&dir, "w1").expect("second state");
        assert!(second.pid.is_some_and(pid_alive));
        assert_ne!(first.pid, second.pid, "respawn installs the new child pid");
        assert!(second.generation.is_some(), "respawn captures a generation");
        stop(&dir, "w1");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unified_state_roundtrips_and_stop_removes_it() {
        let dir = scratch("state");
        let path = state_file(&dir, "w1");
        let record = WatcherGen {
            started_unix: 10,
            binary_mtime_unix: 5,
            binary_len: 77,
            binary_hash: String::new(),
        };
        let state = DoorbellState {
            pid: Some(999_998),
            project: Some("proj".into()),
            generation: Some(record),
        };
        write_state(&path, &state).expect("write state");
        assert_eq!(read_state(&dir, "w1"), Some(state));
        stop(&dir, "w1");
        assert_eq!(read_state(&dir, "w1"), None, "stop removes unified state");
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
            &doorbell_line(EventClass::RequestRelease, "toron next"),
            &record,
        );
        assert!(line.starts_with("[doorbell] request-release."), "{line}");
        assert!(line.contains("Pull: toron next"), "{line}");
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
        write_state(
            &state_file(&dir, "w9"),
            &DoorbellState {
                generation: Some(record),
                ..Default::default()
            },
        )
        .expect("gen");
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
    fn upgrade_cleanup_removes_only_orphaned_legacy_sidecars() {
        let dir = scratch("legacy-cleanup");
        for ws in ["dead", "orphan", "live", "unreadable"] {
            write_gen(
                &gen_file(&dir, ws),
                &WatcherGen {
                    started_unix: 1,
                    binary_mtime_unix: 1,
                    binary_len: 1,
                    binary_hash: String::new(),
                },
            )
            .expect("gen");
            fs::write(project_sidecar(&dir, ws), "proj").expect("project");
        }
        write_pid(&pid_file(&dir, "dead"), 999_998).expect("dead pid");
        write_pid(&pid_file(&dir, "live"), std::process::id()).expect("live pid");
        fs::write(pid_file(&dir, "unreadable"), "not-a-pid\n").expect("bad pid");

        assert_eq!(cleanup_legacy_sidecars(&dir), 5);
        for ws in ["live", "unreadable"] {
            assert!(pid_file(&dir, ws).exists(), "{ws} pid preserved");
            assert!(gen_file(&dir, ws).exists(), "{ws} gen preserved");
            assert!(project_sidecar(&dir, ws).exists(), "{ws} project preserved");
        }
        for ws in ["dead", "orphan"] {
            assert!(!pid_file(&dir, ws).exists(), "{ws} pid removed");
            assert!(!gen_file(&dir, ws).exists(), "{ws} gen removed");
            assert!(!project_sidecar(&dir, ws).exists(), "{ws} project removed");
        }
        assert_eq!(cleanup_legacy_sidecars(&dir), 0, "sweep is idempotent");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_clears_generation_file() {
        let dir = scratch("stop");
        write_state(
            &state_file(&dir, "wst"),
            &DoorbellState {
                generation: Some(WatcherGen {
                    started_unix: 1,
                    binary_mtime_unix: 1,
                    binary_len: 1,
                    binary_hash: String::new(),
                }),
                ..Default::default()
            },
        )
        .expect("gen");
        stop(&dir, "wst");
        assert!(read_state(&dir, "wst").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    /// file stem `swarm_label` writes), NEVER the hc pane-id prefix —
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
            hint.contains("toron next"),
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
        let line = r#"{"kind":9,"id":"17baed1e20b74f424c1c2117bdf7b0b334d928bec923b7194c46cf33116b8b49","tags":[["h","steer-flywheel"],["t","flywheel"],["p","699d97318e69efb667767a917972bebf396d63a10bd6d68bec01e698a903b746"]],"content":"@BrightBadger wake probe4 bd-pe5o"}"#;
        let (class, _) = parse_watch_ndjson(line, &index).expect("mention post must ring");
        assert_eq!(class, EventClass::Channel);
        let bare = r#"{"kind":9,"id":"cc","tags":[["h","steer-flywheel"],["t","flywheel"]],"content":"@BrightBadger ping"}"#;
        let (class, _) = parse_watch_ndjson(bare, &index).expect("@mention must ring");
        assert_eq!(class, EventClass::Channel);
        let chatter = r#"{"kind":9,"id":"dd","tags":[["h","steer-flywheel"],["t","flywheel"]],"content":"hello room"}"#;
        assert!(parse_watch_ndjson(chatter, &index).is_none());
    }

    #[test]
    fn gs006_kind39002_join_leave_rings_when_addressed() {
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
        let msg = crate::doorbell_tick::DoorbellMsg::PlaneStatus {
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
        assert_eq!(all[1].1["msg"]["PlaneStatus"]["pin"], "BlueRidge");
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
        assert!(journal_version_accepted(&serde_json::json!({})));
        assert!(journal_version_accepted(&serde_json::json!({"v": 1})));
        assert!(journal_version_accepted(&serde_json::json!({"v": 2})));
        assert!(!journal_version_accepted(&serde_json::json!({"v": 3})));
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
        assert_eq!(rows[0].state, "unknown");
        assert_eq!(rows[0].recency_secs, None);
        assert_eq!(rows[0].role, "");
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
        let registry = vec![(
            "QuietHeron".to_string(),
            "wP:p2".to_string(),
            "grok".to_string(),
        )];
        let hc = vec![
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
        let rows = join_nearby(&registry, &hc, &leases);
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
        let mut m = emit_services();
        m.remove("ci");
        assert_eq!(
            authorize_emit(&m, "ci", "tok-ci-1", "captain"),
            Err(EmitDeny::UnknownService)
        );
    }

    #[test]
    fn emit_wake_journals_service_row() {
        let tmp = tempfile::tempdir().expect("state dir");
        let dir = tmp.path().to_path_buf();
        service_emit_wake_in(&dir, "proj", "ci", "captain");
        let rows = doorbell_rt::FileStore::new(&dir)
            .fold_workspace("proj")
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["executed"]["stage"], serde_json::json!("emit"));
        assert_eq!(rows[0]["executed"]["service"], serde_json::json!("ci"));
        let pend = dir
            .join("facts")
            .join(format!("{}-pending.jsonl", crate::state::state_key("proj")));
        let body = std::fs::read_to_string(&pend).unwrap_or_default();
        assert!(
            body.contains("emit ci -> captain"),
            "the wake is queued in the given state dir"
        );
    }

    fn seed_home_sub(dir: &std::path::Path, ws: &str, pin: &str) {
        use doorbell_rt::subscribe::*;
        let mut store = SubscriptionStore::open(dir);
        let sub = Subscription {
            id: format!("spawn-{ws}-{pin}"),
            identity: Identity::Pin(pin.to_string()),
            scope: Scope::One(ws.to_string()),
            filter: Filter::default(),
            sink: SinkKind::Hc,
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

    #[test]
    fn inject_journals_row_and_queues_wake() {
        let tmp = tempfile::tempdir().expect("state dir");
        let dir = tmp.path().to_path_buf();
        let proj = "inject-probe";
        let out = inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_000).expect("inject");
        assert!(out.contains('P') && out.contains("hello"));
        let rows = doorbell_rt::FileStore::new(&dir)
            .fold_workspace("manual")
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["executed"]["stage"], serde_json::json!("inject"));
        assert_eq!(rows[0]["executed"]["pin"], serde_json::json!("P"));
        let pend = dir
            .join("facts")
            .join(format!("{}-pending.jsonl", crate::state::state_key(proj)));
        let body = std::fs::read_to_string(&pend).unwrap_or_default();
        assert!(
            body.contains("hello"),
            "one wake queued in the given state dir"
        );
    }

    #[test]
    fn inject_ratchet_refuses_same_second_dup() {
        let tmp = tempfile::tempdir().expect("state dir");
        let dir = tmp.path().to_path_buf();
        let proj = "inject-probe-r";
        inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_000).expect("first");
        assert_eq!(
            inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_000),
            Err(InjectDeny::Ratchet)
        );
        inject_wake_in(&dir, proj, "manual", "P", "hello", 1_700_001).expect("next second");
    }

    #[test]
    fn inject_budget_denies_fourth_in_window() {
        let tmp = tempfile::tempdir().expect("state dir");
        let dir = tmp.path().to_path_buf();
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
        inject_wake_in(&dir, proj, "manual", "P", "later", 1_700_000 + 1801).expect("rolled");
    }

    /// governed-by: bd-z7w0 — resolve_host Embedded arms reachability.
    /// Dead configured URL → TakeLease embedded (+ loud WARNING).
    /// KEEP: fires whenever the configured daemon is down.
    #[test]
    fn degraded_audit_resolve_host_embedded_on_dead_url() {
        let dir = std::env::temp_dir().join(format!("dbll-audit-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
            boot_id: doorbell_rt::lease::boot_id(),
            host: "audit".into(),
            started_unix: now,
            heartbeat_unix: now,
            process_start_unix: doorbell_rt::lease::process_start_unix(std::process::id())
                .unwrap_or(now),
            kind: None,
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
        let measured_tail_ms: u64 = 506;
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

#[cfg(test)]
mod audit_gc_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn write_audit(dir: &Path, workspace: &str) -> PathBuf {
        let path = audit_file(dir, workspace);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"i\":1}\n").unwrap();
        path
    }

    fn record_project(dir: &Path, workspace: &str, project: &str) {
        write_state(
            &state_file(dir, workspace),
            &DoorbellState {
                project: Some(project.to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    }

    /// A dispatcher that is still up owns its file, however old it looks. The
    /// pid is the only signal that says "this workspace will be served again".
    #[test]
    fn a_live_dispatcher_keeps_its_audit() {
        let dir = tempfile::tempdir().unwrap();
        write_audit(dir.path(), "live");
        write_state(
            &state_file(dir.path(), "live"),
            &DoorbellState {
                pid: Some(4242),
                ..Default::default()
            },
        )
        .unwrap();
        let found = dead_audit_files(dir.path(), 0, SystemTime::now(), |pid| pid == 4242);
        assert!(found.is_empty(), "{found:?}");
    }

    /// A sidecar that records an absolute project path which no longer exists
    /// is a finished gate run's temp root. It is dead now, not in 30 days: the
    /// workspace can never be started again.
    #[test]
    fn a_vanished_project_path_is_dead_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        write_audit(dir.path(), "flywheel-fw-gate-1");
        record_project(
            dir.path(),
            "flywheel-fw-gate-1",
            "/tmp/gone-forever-fw-gate-1",
        );
        let found = dead_audit_files(dir.path(), 30, SystemTime::now(), |_| false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].reason, "project-gone");
        assert_eq!(found[0].workspace, "flywheel-fw-gate-1");
    }

    /// A bare project name (how `doorbell start` records a human project)
    /// cannot be tested for existence, so it prunes on age only. Fresh and
    /// quiet is kept; past the horizon it goes.
    #[test]
    fn a_bare_project_name_only_prunes_on_age() {
        let dir = tempfile::tempdir().unwrap();
        write_audit(dir.path(), "flywheel");
        record_project(dir.path(), "flywheel", "flywheel");
        let mtime = std::fs::metadata(audit_file(dir.path(), "flywheel"))
            .unwrap()
            .modified()
            .unwrap();

        let fresh = dead_audit_files(dir.path(), 30, mtime + Duration::from_secs(86_400), |_| {
            false
        });
        assert!(
            fresh.is_empty(),
            "a quiet week must not delete a project log"
        );

        let idle = dead_audit_files(
            dir.path(),
            30,
            mtime + Duration::from_secs(31 * 86_400),
            |_| false,
        );
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].reason, "idle");
    }

    /// Only `.audit.jsonl` is a candidate: the small per-workspace status
    /// files stay, so `doorbell status` still knows the workspace existed.
    #[test]
    fn siblings_and_other_suffixes_are_never_candidates() {
        let dir = tempfile::tempdir().unwrap();
        write_audit(dir.path(), "ws");
        std::fs::write(dir.path().join("doorbell").join("ws.audit.loudmiss"), "x").unwrap();
        std::fs::write(dir.path().join("doorbell").join("ws.gen"), "x").unwrap();
        record_project(dir.path(), "ws", "/tmp/gone-forever-ws");
        let found = dead_audit_files(dir.path(), 30, SystemTime::now(), |_| false);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.ends_with("ws.audit.jsonl"));
    }

    /// gc/one-rule (bd-io2ws): the on-demand door and the daemon's hourly
    /// prune must reach the SAME verdict, because they share one pure
    /// predicate (`doorbell_rt::store::audit_abandon_reason`). Three shapes:
    /// vanished project (dead at any age), idle past the horizon, young.
    #[test]
    fn doorbell_gc_and_daemon_prune_share_one_rule() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let now_secs = now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        write_audit(dir.path(), "project-gone");
        record_project(dir.path(), "project-gone", "/tmp/gone-forever-io2ws");
        write_audit(dir.path(), "idle");
        record_project(dir.path(), "idle", "idle");
        let old = now - Duration::from_secs(40 * 86_400);
        std::fs::File::open(audit_file(dir.path(), "idle"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        write_audit(dir.path(), "young");

        let found = dead_audit_files(dir.path(), 30, now, |_| false);
        let gc: std::collections::BTreeMap<&str, &str> = found
            .iter()
            .map(|f| (f.workspace.as_str(), f.reason))
            .collect();
        assert_eq!(gc.get("project-gone"), Some(&"project-gone"));
        assert_eq!(gc.get("idle"), Some(&"idle"));
        assert!(!gc.contains_key("young"), "a young log is not a candidate");

        let live = std::collections::BTreeSet::new();
        let mtime = |ws: &str| {
            std::fs::metadata(audit_file(dir.path(), ws))
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        let daemon = |ws: &str, project_gone: bool| {
            doorbell_rt::store::audit_abandon_reason(
                ws,
                mtime(ws),
                project_gone,
                &live,
                30 * 86_400,
                now_secs,
            )
        };
        assert_eq!(
            daemon("project-gone", true),
            Some(doorbell_rt::store::AuditAbandon::ProjectGone)
        );
        assert_eq!(
            daemon("idle", false),
            Some(doorbell_rt::store::AuditAbandon::Idle)
        );
        assert_eq!(daemon("young", false), None);
    }
}

#[cfg(test)]
mod audit_rotation_tests {
    use super::*;

    /// The live dispatcher appends audit rows for hours, so the file must be
    /// byte-bounded even though `audit_append` is not the doorbell-rt store.
    /// Before the rotation call, one workspace file reached 17 MB.
    #[test]
    fn audit_append_stays_under_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let filler = "x".repeat(900);
        for i in 0..8_000 {
            audit_append(dir.path(), "ws", serde_json::json!({"i": i, "f": filler}));
        }
        let path = audit_file(dir.path(), "ws");
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(
            len <= doorbell_rt::file_host::AUDIT_MAX_BYTES + 4096,
            "audit file is {len} bytes, over the {} byte cap",
            doorbell_rt::file_host::AUDIT_MAX_BYTES
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"i\":7999"),
            "the newest row was rotated away"
        );
    }
}

/// bd-gefp: a host must not outlive the workspace it watches.
#[cfg(test)]
mod bd_gefp_host_exit_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fw-gefp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The bead's witness: the host retires only on a sustained `Gone` read,
    /// and journals WHY. `Unreadable` never retires a live host (a plane
    /// mid-restart is not an absent workspace), and one `Present` resets the
    /// streak, so a flicker cannot accumulate into an exit.
    #[test]
    fn doorbell_host_exits_when_its_workspace_is_gone() {
        let dir = scratch("exit");
        let ws = "flywheel-scratch-100927";
        let mut seen = 0u32;

        assert_eq!(
            host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Present),
            None
        );
        assert_eq!(seen, 0);
        assert_eq!(
            host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Unreadable),
            None
        );
        assert_eq!(
            host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Gone),
            None,
            "one Gone is a plane hiccup, not an exit"
        );
        assert_eq!(
            host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Present),
            None
        );
        assert_eq!(seen, 0);
        assert_eq!(
            host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Gone),
            None
        );
        assert_eq!(
            host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Gone),
            None
        );
        let reason = host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Gone)
            .expect("WORKSPACE_GONE_STREAK consecutive Gone reads retire the host");
        assert_eq!(reason, "workspace-gone");

        let store = doorbell_rt::FileStore::new(dir.clone());
        let audit = std::fs::read_to_string(store.audit_file(ws))
            .expect("the exit reason must be journaled, not just printed");
        assert!(audit.contains("\"kind\":\"host_exit\""), "audit: {audit}");
        assert!(
            audit.contains("\"reason\":\"workspace-gone\""),
            "audit: {audit}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Empty listings prove nothing: a plane mid-restart must read
    /// `Unreadable`, never `Gone`. Scripted lookups: one miss survives,
    /// only sustained confirmed `Gone` retires.
    #[test]
    fn transient_miss_never_retires() {
        assert_eq!(
            classify_list_presence(true, false),
            WorkspacePresence::Unreadable
        );
        assert_eq!(
            classify_list_presence(false, false),
            WorkspacePresence::Gone
        );
        assert_eq!(
            classify_list_presence(false, true),
            WorkspacePresence::Present
        );
        let dir = scratch("scripted");
        let ws = "flywheel-scratch-scripted";
        let mut seen = 0u32;
        let script = [
            WorkspacePresence::Gone,
            WorkspacePresence::Unreadable,
            WorkspacePresence::Gone,
            WorkspacePresence::Gone,
            WorkspacePresence::Present,
            WorkspacePresence::Gone,
            WorkspacePresence::Gone,
        ];
        for presence in script {
            assert_eq!(host_exit_check(&dir, ws, &mut seen, presence), None);
        }
        let reason = host_exit_check(&dir, ws, &mut seen, WorkspacePresence::Gone)
            .expect("three consecutive Gone retire");
        assert_eq!(reason, "workspace-gone");
        let store = doorbell_rt::FileStore::new(dir.clone());
        let audit = std::fs::read_to_string(store.audit_file(ws)).expect("exit must journal");
        assert!(audit.contains("\"streak\":3"), "audit: {audit}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wiring, not just the decision: the embedded host's liveness poll
    /// must CALL the shared check. Deleting the call (or reverting to a bare
    /// eprintln) must not stay green.
    #[test]
    fn the_liveness_poll_calls_the_shared_exit_check() {
        let src = include_str!("doorbell_host.rs")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            src.contains(
                "doorbell::host_exit_check(&probe_dir, &probe_ws, &mut gone_streak, presence)"
            ),
            "the liveness poll must evaluate and journal through host_exit_check"
        );
        assert!(
            src.contains("doorbell::probe_workspace_presence(&cfg_p, &project_p, &ws_p)"),
            "the poll must ask the SELECTED plane, not read a stale local file"
        );
        assert!(
            src.contains("doorbell::retire_workspace"),
            "a confirmed workspace-gone retire must drop the state record, or supervise re-arms it forever"
        );
    }

    /// `doorbell_project_workspaces` arms every workspace with a state file.
    /// A retired host must remove its own record, and must not remove a
    /// replacement that already rewrote it.
    #[test]
    fn a_retired_workspace_drops_its_state_record() {
        let dir = scratch("retire");
        let ws = "flywheel-scratch-retire";
        write_state(
            &state_file(&dir, ws),
            &DoorbellState {
                pid: Some(4242),
                project: Some("toron".into()),
                generation: None,
            },
        )
        .unwrap();
        assert!(read_state(&dir, ws).is_some());
        retire_workspace(&dir, ws, 4242);
        assert!(
            read_state(&dir, ws).is_none(),
            "a retained record is re-armed by the next supervise tick"
        );
        write_state(
            &state_file(&dir, ws),
            &DoorbellState {
                pid: Some(7),
                project: Some("toron".into()),
                generation: None,
            },
        )
        .unwrap();
        retire_workspace(&dir, ws, 4242);
        assert_eq!(
            read_state(&dir, ws).and_then(|s| s.pid),
            Some(7),
            "a retired pid must not delete the replacement host's record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dropping a `Child` does not reap it, so a repeating supervisor kept one
    /// `<defunct>` entry per respawn. `spawn_child` must hand the child to a
    /// waiter: after the child exits, `ps` must have no entry at all (an
    /// unreaped child would still show `Z`).
    #[test]
    fn a_spawned_child_is_reaped() {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::PermissionsExt as _;
            let dir = scratch("reap");
            let script = dir.join("slow-child.sh");
            let mut f = std::fs::File::create(&script).unwrap();
            writeln!(f, "#!/bin/sh\nsleep 0.2").unwrap();
            drop(f);
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

            spawn_child(&script, &dir, "proj", "ws").unwrap();
            let pid = read_state(&dir, "ws")
                .and_then(|s| s.pid)
                .expect("spawn_child records its pid");
            let stat = |pid: u32| -> String {
                let out = std::process::Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "stat="])
                    .output()
                    .map(|o| o.stdout)
                    .unwrap_or_default();
                String::from_utf8_lossy(&out).trim().to_string()
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !stat(pid).is_empty() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            assert!(
                stat(pid).is_empty(),
                "pid {pid} is still in the process table (zombie = unreaped): {}",
                stat(pid)
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

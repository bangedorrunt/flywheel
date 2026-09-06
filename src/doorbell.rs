// governed-by: ADR-0025
// governed-by: ADR-0005
//! Workspace doorbell (ADR-0005 D1): ring rules, queue coalesce, inject
//! gate. The live dispatcher process is spawn-owned; this module is the
//! functional core the e2e gate tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// One class of doorbell. Queued last-write-wins per class (ADR-0005 D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventClass {
    Channel,
    Job,
    RequestRelease,
    ReservationConflict,
    PlanePUrgent,
}

/// A watch/inbox edge the dispatcher might ring on.
#[derive(Debug, Clone)]
pub struct RingInput {
    pub kind: u16,
    /// Kind-9 `p` / job assignee / request-release target is this pin.
    pub addressed_to_self: bool,
    pub toron_kind_request_release: bool,
    pub toron_kind_mail_delivered: bool,
    pub importance_high_or_urgent: bool,
    pub reservation_conflicts_self: bool,
    /// Room chatter: kind 9 with no self address.
    pub is_presence: bool,
    pub is_ack_only: bool,
}

/// Classify a bus/inbox edge. `None` = do not ring.
pub fn classify(ev: &RingInput) -> Option<EventClass> {
    if ev.is_presence || ev.is_ack_only {
        return None;
    }
    if ev.toron_kind_request_release && ev.addressed_to_self {
        return Some(EventClass::RequestRelease);
    }
    if ev.reservation_conflicts_self && ev.kind == 30031 {
        return Some(EventClass::ReservationConflict);
    }
    if ev.addressed_to_self && ev.kind == 43001 {
        return Some(EventClass::Job);
    }
    // GS-006 bug 1: kind 9 mail-delivered (NIP-17 gift-wrap wake) must pull mail, not channel.
    // Tagged `toron-kind=mail-delivered` on kind 9; reuse PlanePUrgent (inbox pull) path.
    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_mail_delivered {
        return Some(EventClass::PlanePUrgent);
    }
    if ev.addressed_to_self && ev.kind == 9 {
        return Some(EventClass::Channel);
    }
    // GS-006 bug 2: kind 39002 join/leave addressed to self must ring.
    // Reuse Channel semantics (membership wake) — hint falls back to channel read.
    if ev.addressed_to_self && ev.kind == 39002 {
        return Some(EventClass::Channel);
    }
    if ev.importance_high_or_urgent && ev.addressed_to_self {
        return Some(EventClass::PlanePUrgent);
    }
    None
}

/// Inject only when the pane is idle, done, or blocked (blocked is await-signal,
/// not "don't disturb" — a peer's fix must break through, ADR-0005 gap 5).
pub fn should_inject(herdr_status: &str) -> bool {
    matches!(herdr_status, "idle" | "done" | "blocked")
}

/// Drain when leaving a non-injectable state for an injectable one.
/// Working is the only non-injectable state; idle|done|blocked are injectable.
pub fn is_drain_transition(from: &str, to: &str) -> bool {
    !should_inject(from) && should_inject(to)
}

/// Last-write-wins queue, one line per (EventClass, hint/channel).
/// Gap 6 fix: per-channel coalesce — two channels while working no longer collide.
#[derive(Debug, Default)]
pub struct DoorbellQueue {
    pending: BTreeMap<(EventClass, String), String>,
}

impl DoorbellQueue {
    pub fn push(&mut self, class: EventClass, line: impl Into<String>) {
        let line = line.into();
        // hint is the Pull part; use full line as key fallback when hint unknown.
        // For correct per-channel dedup, callers pass the hint-derived line.
        // Extract the hint suffix for keying when possible.
        let hint = line.split("Pull: ").nth(1).unwrap_or(&line).to_string();
        self.pending.insert((class, hint), line);
    }

    /// Direct insertion with explicit channel/hint key (preserves per-channel).
    #[allow(dead_code)]
    pub fn push_with_hint(&mut self, class: EventClass, hint: String, line: String) {
        self.pending.insert((class, hint), line);
    }

    #[allow(dead_code)] // asserted by queue coalesce tests
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Drain in class+hint order. Empty after.
    pub fn drain(&mut self) -> Vec<(EventClass, String)> {
        if self.is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|((class, _), line)| (class, line))
            .collect()
    }
}

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

/// One-line identity prime. Injected even with `--no-peer-aware` (D1 item 1).
pub fn identity_prime(name: &str, mail_identity: &str) -> String {
    format!(
        "You are {name}. Your mail identity is {mail_identity}. Guard will block unattributed/unreserved commits."
    )
}

/// Per-pin 5/30/3 budget the dispatcher enforces (D1 item 2).
/// Rolling token bucket: 3 prompts per 30-min window, refilled continuously.
/// Gap 4 fix: long crews no longer deafen after 30m.
#[derive(Debug, Clone)]
pub struct PinBudget {
    pub budget_minutes: u32,
    pub max_prompts: u32,
    /// unix secs of each granted prompt (rolling window)
    pub prompts: Vec<u64>,
    #[allow(dead_code)]
    pub started_unix: u64,
}

impl PinBudget {
    pub fn adr_default(started_unix: u64) -> Self {
        Self::new(30, 3, started_unix)
    }

    pub fn new(budget_minutes: u32, max_prompts: u32, started_unix: u64) -> Self {
        Self {
            budget_minutes,
            max_prompts,
            prompts: Vec::new(),
            started_unix,
        }
    }

    fn window_secs(&self) -> u64 {
        u64::from(self.budget_minutes) * 60
    }

    pub fn allows(&self, now_unix: u64) -> bool {
        let window = self.window_secs();
        let recent = self
            .prompts
            .iter()
            .filter(|t| now_unix.saturating_sub(**t) < window)
            .count();
        (recent as u32) < self.max_prompts
    }

    #[allow(dead_code)]
    pub fn record(&mut self) {
        let now = unix_now();
        self.record_at(now);
    }

    pub fn record_at(&mut self, now_unix: u64) {
        let window = self.window_secs();
        self.prompts
            .retain(|t| now_unix.saturating_sub(*t) < window);
        self.prompts.push(now_unix);
    }
}

/// Drop injects that exceed the pin's 5/30/3 budget (rolling).
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
                    // Caller that has exe should use inspect_lock_generation_with_hash.
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

/// Hash-aware variant that also checks the binary sha1 when the record carries it.
#[allow(dead_code)]
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
pub fn stop(state_dir: &Path, workspace: &str) -> Result<()> {
    let path = pid_file(state_dir, workspace);
    if let Some(pid) = read_pid(&path) {
        kill_pid(pid);
    }
    clear_pid(&path);
    let _ = fs::remove_file(unanswered_file(state_dir, workspace));
    let _ = fs::remove_file(gen_file(state_dir, workspace));
    Ok(())
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
    let path = pid_file(state_dir, workspace);
    let gen_path = gen_file(state_dir, workspace);
    let action = inspect_lock_generation(&path, &gen_path, binary_identity(exe), pid_alive);
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

/// Fan-out: given classified rings and pane status, produce (pin, line)
/// pairs that should be prompted now. Working pins are skipped (queued
/// by the caller).
#[allow(dead_code)]
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

/// Pins this dispatcher owns. Live kind-9 `p` tags are **npub hex**, not
/// the pin; jobs use `assignee=<pin>`. Resolve both to a pin.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SelfIndex {
    pins: BTreeMap<String, String>,
    npubs: BTreeMap<String, String>,
}

impl SelfIndex {
    pub fn from_pins<I, S>(pins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut idx = Self::default();
        for p in pins {
            idx.insert_pin(p.into(), None);
        }
        idx
    }

    pub fn insert_pin(&mut self, pin: String, npub: Option<String>) {
        if let Some(n) = npub.filter(|s| !s.is_empty()) {
            let n = n.to_lowercase();
            self.npubs.insert(n.clone(), pin.clone());
            self.pins.insert(pin, n);
        } else {
            self.pins.entry(pin).or_default();
        }
    }

    pub fn resolve(&self, token: &str) -> Option<String> {
        if self.pins.contains_key(token) {
            return Some(token.to_string());
        }
        self.npubs.get(&token.to_lowercase()).cloned()
    }

    pub fn pin_names(&self) -> impl Iterator<Item = &str> {
        self.pins.keys().map(String::as_str)
    }

    pub fn npub(&self, pin: &str) -> Option<&str> {
        let n = self.pins.get(pin)?;
        if n.is_empty() { None } else { Some(n.as_str()) }
    }
}

/// Seconds after inject with no agent-originated event before the pin is
/// unanswered (presence / kind-9 / reaction from their npub).
pub const UNANSWERED_SECS: u64 = 120;

/// bd-durable-doorbell-lngf: when NO wake-state high-water exists (first
/// run, or state lost), the inbox sweep looks this far back instead of
/// `now` — mail that arrived while the dispatcher was dead still wakes.
/// Bounded so a never-seen inbox does not replay forever; the `[doorbell]`
/// line + per-pin budget keep the replay idempotent and cheap.
pub const REDELIVERY_LOOKBACK_HOURS: u64 = 24;

#[derive(Debug, Clone)]
struct PendingRing {
    npub: String,
    injected_unix: u64,
}

/// Heard-or-deaf: a ring that is not heard is a conflict-shaped event.
#[derive(Debug, Default, Clone)]
pub struct HearTracker {
    pending: BTreeMap<String, PendingRing>,
    unanswered: BTreeSet<String>,
    /// pin → npub, kept after expire so a late post still clears deaf.
    npubs: BTreeMap<String, String>,
}

impl HearTracker {
    pub fn note_inject(&mut self, pin: &str, npub: Option<&str>, now: u64) {
        self.unanswered.remove(pin);
        let npub = npub.unwrap_or("").to_lowercase();
        if !npub.is_empty() {
            self.npubs.insert(pin.to_string(), npub.clone());
        }
        self.pending.insert(
            pin.to_string(),
            PendingRing {
                npub,
                injected_unix: now,
            },
        );
    }

    /// Fill npubs learned later (whois on the 1s tick).
    pub fn remember_npub(&mut self, pin: &str, npub: Option<&str>) {
        let Some(n) = npub.filter(|s| !s.is_empty()) else {
            return;
        };
        let n = n.to_lowercase();
        self.npubs.insert(pin.to_string(), n.clone());
        if let Some(ring) = self.pending.get_mut(pin) {
            ring.npub = n;
        }
    }

    pub fn note_heard_pin(&mut self, pin: &str) {
        self.pending.remove(pin);
        self.unanswered.remove(pin);
    }

    pub fn note_heard_pubkey(&mut self, pubkey: &str) {
        let key = pubkey.to_lowercase();
        let mut pins: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, r)| !r.npub.is_empty() && r.npub == key)
            .map(|(p, _)| p.clone())
            .collect();
        pins.extend(
            self.npubs
                .iter()
                .filter(|(_, n)| *n == &key)
                .map(|(p, _)| p.clone()),
        );
        pins.sort();
        pins.dedup();
        for pin in pins {
            self.note_heard_pin(&pin);
        }
    }

    /// Move timed-out pending pins to unanswered. Returns newly expired pins.
    pub fn expire(&mut self, now: u64, timeout_secs: u64) -> Vec<String> {
        let mut expired = Vec::new();
        self.pending.retain(|pin, ring| {
            if now.saturating_sub(ring.injected_unix) >= timeout_secs {
                expired.push(pin.clone());
                false
            } else {
                true
            }
        });
        for pin in &expired {
            self.unanswered.insert(pin.clone());
        }
        expired
    }

    pub fn unanswered_pins(&self) -> Vec<String> {
        self.unanswered.iter().cloned().collect()
    }

    pub fn seed_unanswered(&mut self, pins: impl IntoIterator<Item = String>) {
        self.unanswered.extend(pins);
    }
}

pub fn unanswered_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("{workspace}.unanswered"))
}

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

/// Poster pubkey on a `toron watch` NDJSON line.
pub fn watch_pubkey(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("pubkey")
        .and_then(|p| p.as_str())
        .map(str::to_lowercase)
}

/// Event id on a watch NDJSON line (gap 17 trust: hint carries proof).
pub fn watch_id(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("id")
        .and_then(|p| p.as_str())
        .map(std::string::ToString::to_string)
}

/// `created_at` unix secs on a watch NDJSON line (gap 1 fix: plane-S cursor).
#[allow(dead_code)]
pub fn watch_created_at(line: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("created_at").and_then(serde_json::Value::as_u64)
}

/// `dispatcher: up|down` plus watcher generation (bd-stale-doorbell-wake-0fic)
/// plus unanswered pins (digest / `--report`). The generation fields let an
/// agent compare the banner stamp against the pid-file owner.
/// bd-durable-doorbell-lngf: persisted per-workspace wake state. The
/// dispatcher loads it at start and saves after every processed wake — a
/// crash loses nothing that matters: plane-P/S high-waters survive (no replay
/// storms), the last-wake stamp feeds `status` liveness, and redelivery is
/// at-least-once with an idempotent `[doorbell]` line.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WakeState {
    /// plane-P inbox high-water per pin (RFC3339) — survives restarts.
    #[serde(default)]
    pub since_by_pin: BTreeMap<String, String>,
    /// plane-S high-water (unix micros of newest seen watch event) — gap 1 fix.
    #[serde(default)]
    pub plane_s_cursor_us: Option<i64>,
    /// last successful wake injection (unix secs) — status liveness.
    #[serde(default)]
    pub last_wake_unix: Option<u64>,
    #[serde(default)]
    pub last_wake_pin: Option<String>,
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

/// Record one delivered wake (the stamp `status` liveness reads).
pub fn note_wake(st: &mut WakeState, pin: &str, now_unix: u64) {
    st.last_wake_unix = Some(now_unix);
    st.last_wake_pin = Some(pin.to_string());
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

/// Pins addressed by this watch line: `p`/`assignee` (pin or npub) plus
/// kind-9 `@pin` in content (live posts do both; `p` is npub).
pub fn watch_targets(line: &str, index: &SelfIndex) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let mut out = BTreeSet::new();
    if let Some(tags) = v.get("tags").and_then(|t| t.as_array()) {
        for t in tags {
            let Some(a) = t.as_array() else { continue };
            let Some(key) = a.first().and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(val) = a.get(1).and_then(|x| x.as_str()) else {
                continue;
            };
            if matches!(key, "p" | "assignee")
                && let Some(pin) = index.resolve(val)
            {
                out.insert(pin);
            }
        }
    }
    if v.get("kind").and_then(serde_json::Value::as_u64) == Some(9)
        && let Some(content) = v.get("content").and_then(|c| c.as_str())
    {
        for pin in index.pin_names() {
            if content.contains(&format!("@{pin}")) {
                out.insert(pin.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Parse one `toron watch` NDJSON line (`{kind,id,tags,content}`).
pub fn parse_watch_ndjson(line: &str, index: &SelfIndex) -> Option<(EventClass, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = v.get("kind")?.as_u64()? as u16;
    let tags = v.get("tags")?.as_array()?;
    let addressed_to_self = !watch_targets(line, index).is_empty();
    let toron_kind_request_release = tags.iter().any(|t| {
        t.as_array()
            .map(|a| {
                a.first().and_then(|x| x.as_str()) == Some("toron-kind")
                    && a.get(1).and_then(|x| x.as_str()) == Some("request-release")
            })
            .unwrap_or(false)
    });
    // mail-delivered AND ack-sla-expired are mailbox wakes (kind 9 metadata
    // rings for sealed NIP-17 mail) — both must pull the inbox, never
    // `channel read` on a thread id.
    let toron_kind_mail_delivered = tags.iter().any(|t| {
        t.as_array()
            .map(|a| {
                a.first().and_then(|x| x.as_str()) == Some("toron-kind")
                    && matches!(
                        a.get(1).and_then(|x| x.as_str()),
                        Some("mail-delivered" | "ack-sla-expired")
                    )
            })
            .unwrap_or(false)
    });
    let class = classify(&RingInput {
        kind,
        addressed_to_self,
        toron_kind_request_release,
        toron_kind_mail_delivered,
        importance_high_or_urgent: false,
        reservation_conflicts_self: kind == 30031 && addressed_to_self,
        is_presence: kind == 20001,
        is_ack_only: false,
    })?;
    let h = tags.iter().find_map(|t| {
        let a = t.as_array()?;
        if a.first()?.as_str()? == "h" {
            a.get(1)?.as_str()
        } else {
            None
        }
    });
    let hint = pull_hint(class, h);
    Some((class, hint))
}

fn pull_hint(class: EventClass, channel: Option<&str>) -> String {
    match class {
        EventClass::Channel => match channel {
            Some(h) => format!("toron channel read h={h}"),
            None => "toron channel read".into(),
        },
        EventClass::Job => "toron job list".into(),
        EventClass::RequestRelease | EventClass::PlanePUrgent => "toron mail inbox --since".into(),
        EventClass::ReservationConflict => "toron reserve list".into(),
    }
}

/// Classify one `fetch_inbox` row (plane P). Inbox is already pin-scoped.
pub fn parse_inbox_item(v: &serde_json::Value) -> Option<(EventClass, String)> {
    if v.get("ack_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let tags = v.get("tags").and_then(|t| t.as_array());
    let request_release = tags
        .map(|tags| {
            tags.iter().any(|t| {
                t.as_array()
                    .map(|a| {
                        a.first().and_then(|x| x.as_str()) == Some("toron-kind")
                            && a.get(1).and_then(|x| x.as_str()) == Some("request-release")
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let importance = v.get("importance").and_then(|x| x.as_str()).unwrap_or("");
    let class = if request_release {
        EventClass::RequestRelease
    } else if matches!(importance, "high" | "urgent") {
        EventClass::PlanePUrgent
    } else {
        return None;
    };
    Some((class, pull_hint(class, None)))
}

/// Rumor-time of one inbox JSON row: live ToJson uses `created_ts_us` (i64
/// micros); CLI/legacy blobs may still carry `created_ts` RFC3339.
pub fn row_created_us(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.get("created_ts_us") {
        if let Some(i) = n.as_i64() {
            return Some(i);
        }
        if let Some(u) = n.as_u64() {
            return Some(u as i64);
        }
        if let Some(s) = n.as_str() {
            return s.parse().ok();
        }
    }
    v.get("created_ts")
        .and_then(|t| t.as_str())
        .and_then(|s| parse_rfc3339_micros(s).ok())
}

fn us_to_rfc3339(us: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_micros(us)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

fn inbox_items(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(arr) = v.as_array() {
        arr.iter().collect()
    } else if let Some(arr) = v.get("messages").and_then(|m| m.as_array()) {
        arr.iter().collect()
    } else if v.is_object() {
        vec![v]
    } else {
        Vec::new()
    }
}

pub fn inbox_high_water_from_values(items: Vec<&serde_json::Value>) -> Option<String> {
    let mut best: Option<(i64, String)> = None;
    for i in items {
        let Some(us) = row_created_us(i) else {
            continue;
        };
        let label = i
            .get("created_ts")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .or_else(|| us_to_rfc3339(us));
        let Some(label) = label else {
            continue;
        };
        if best.as_ref().map(|(u, _)| us > *u).unwrap_or(true) {
            best = Some((us, label));
        }
    }
    best.map(|(_, s)| s)
}

/// Newest created time in an inbox blob as RFC3339, for the high-water.
/// Compares numerically (micros). Live JSON is `created_ts_us`.
pub fn inbox_high_water(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    inbox_high_water_from_values(inbox_items(&v))
}

/// Newest created time as unix-micros (None if unparseable).
pub fn inbox_high_water_micros(raw: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    inbox_items(&v).into_iter().filter_map(row_created_us).max()
}

pub fn parse_rfc3339_micros(ts: &str) -> Result<i64, chrono::ParseError> {
    chrono::DateTime::parse_from_rfc3339(ts).map(|dt| dt.timestamp_micros())
}

/// One `[doorbell]` line. Body stays pull.
pub fn doorbell_line(class: EventClass, hint: &str) -> String {
    let kind = match class {
        EventClass::Channel => "kind 9",
        EventClass::Job => "kind 43001",
        EventClass::RequestRelease => "request-release",
        EventClass::ReservationConflict => "kind 30031",
        EventClass::PlanePUrgent => "plane-P urgent",
    };
    format!("[doorbell] {kind}. Pull: {hint}")
}

/// Trust-aware line that carries the triggering event id for proof (gap 17).
pub fn doorbell_line_with_event(class: EventClass, hint: &str, event_id: &str) -> String {
    if event_id.is_empty() {
        doorbell_line(class, hint)
    } else {
        let kind = match class {
            EventClass::Channel => "kind 9",
            EventClass::Job => "kind 43001",
            EventClass::RequestRelease => "request-release",
            EventClass::ReservationConflict => "kind 30031",
            EventClass::PlanePUrgent => "plane-P urgent",
        };
        format!("[doorbell] {kind} event={event_id}. Pull: {hint}")
    }
}

/// Parse `--status pin=idle` pairs for the herdr-mock (`__doorbell --once`).
pub fn parse_status_pair(s: &str) -> Option<(String, String)> {
    let (pin, st) = s.split_once('=')?;
    if pin.is_empty() || st.is_empty() {
        return None;
    }
    Some((pin.to_string(), st.to_string()))
}

/// Second reducers deleted per bd-3wzl.
/// Live dispatcher now routes every watch/status edge through `workspace::tick` (tick(&WorkspaceModel, &Msg)).
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

/// One journaled tick: the Msg in, the Actions out, what the effect layer
/// actually did. `doorbell why <pin>` replays this — never log tailing.
pub fn journal_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("journals")
        .join(format!("doorbell-journal-{workspace}.jsonl"))
}

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
    // Bounded retention: doorbell journal also grows without bound — same
    // 5 MiB / 5k line cap as loop journal. Keep recent history for `why`.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 5 * 1024 * 1024 {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = raw.lines().collect();
                if lines.len() > 5000 {
                    let keep = lines[lines.len() - 5000..].join("\n") + "\n";
                    let _ = std::fs::write(&path, keep);
                }
            }
        }
    }
}

/// Workspace-tick journal entry writer. `pins` is the inject-relevant pin set
/// (bd-z54w honesty contract): the fold restores the SelfIndex from it so
/// classification reproduces deterministically. `None` (or entries predating
/// the field) fold with Unknown-presence semantics — the fold never invents
/// idle. Serde-side the field is plain JSON (`null` when absent); readers use
/// `entry.get("pins")` with missing/null meaning unknown.
pub fn journal_append_workspace(
    state_dir: &Path,
    workspace: &str,
    msg: &crate::workspace::Msg,
    cmds: &[crate::workspace::Cmd],
    executed: serde_json::Value,
    pins: Option<&[String]>,
) {
    let path = journal_file(state_dir, workspace);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entry = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "workspace": workspace,
        "msg": msg,
        "actions": cmds,
        "executed": executed,
        "pins": pins,
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{entry}");
    }
    // Same 5 MiB / 5k line cap — workspace journal is the same file.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 5 * 1024 * 1024 {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = raw.lines().collect();
                if lines.len() > 5000 {
                    let keep = lines[lines.len() - 5000..].join("\n") + "\n";
                    let _ = std::fs::write(&path, keep);
                }
            }
        }
    }
}

/// All journal entries across every workspace, oldest first. Missing or
/// corrupt lines are skipped (a journal is a trust surface, not a WAL).
pub fn journal_read_all(state_dir: &Path) -> Vec<(String, serde_json::Value)> {
    let dir = state_dir.join("journals");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("doorbell-journal-") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let workspace = p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                n.trim_start_matches("doorbell-journal-")
                    .trim_end_matches(".jsonl")
                    .to_string()
            })
            .unwrap_or_default();
        if let Ok(raw) = std::fs::read_to_string(&p) {
            for line in raw.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    out.push((workspace.clone(), v));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn base() -> RingInput {
        RingInput {
            kind: 9,
            addressed_to_self: false,
            toron_kind_request_release: false,
            toron_kind_mail_delivered: false,
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
    fn sc_d1_kind9_at_pin_injects_when_idle() {
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
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].0, "BlueRidge");
        assert!(prompts[0].1.contains("[doorbell]"));
        assert!(next.doorbell.queues.is_empty(), "idle pin must not queue");
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
    fn s_proof_why_producer_retention_bound() {
        // zfnx.9 producer contract: 5MiB / 5k-line retention cap on the live path.
        let src = include_str!("doorbell.rs");
        assert!(
            src.contains("5 * 1024 * 1024"),
            "journal retention must keep the 5MiB bound"
        );
        assert!(
            src.contains("5000"),
            "journal retention must keep the 5k-line bound"
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
            line: kind9_at_blueridge().to_string(),
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
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, "BlueRidge");
        assert!(drained[0].1.contains("[doorbell]"));
        assert!(
            next2
                .doorbell
                .queues
                .get("BlueRidge")
                .is_none_or(Vec::is_empty)
        );
    }

    #[test]
    fn sc_d1_kind9_npub_p_tag_rings_pin() {
        let npub = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let line = format!(
            r#"{{"kind":9,"id":"ab","tags":[["p","{npub}"],["h","steer-x"]],"content":"@BlueRidge hi"}}"#
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
            "content @pin must ring even when p is npub"
        );
        assert_eq!(prompts[0].0, "BlueRidge");
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
        stop(&dir, "w1").expect("stop");
        assert!(!pid_file(&dir, "w1").exists());
        let _ = std::fs::remove_dir_all(&dir);
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
        stop(&dir, "w1").expect("stop");
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
        stop(&dir, "wst").expect("stop");
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
            EventClass::PlanePUrgent,
            "mail-delivered should reuse PlanePUrgent mail pull"
        );
        assert!(
            hint.contains("toron mail inbox"),
            "hint must be mail inbox, got {hint}"
        );
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
}

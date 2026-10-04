// governed-by: ADR-0012
//! FileHost — daemon-native [`Host`](crate::host::Host) (bd-ene2 Phase 3).
//!
//! Same [`Brain`], file/CLI inputs: flywheel state files (spawn registry,
//! role binds, pending role-bounds/facts, pid/gen) plus the `toron` /
//! `herdr` CLIs (inbox, whois, prompt, agent list) and the daemon
//! `wake.sock` subscription (no `toron watch` child). No flywheel types —
//! this is what lets the `doorbell` daemon host the loop without linking
//! the orchestrator.
//!
//! Parity contract (bd-ene2 verify): the same [`BrainMsg`] sequence with
//! the same registry/presence/inbox inputs yields the same [`Effect`]
//! sequence as the embedded `RtHost`. Rules: classification is
//! `doorbell-core` verbatim (via [`CoreReducer`]); the presence prompt
//! filter mirrors `workspace::filter_prompts` (Idle keeps, Busy+urgent
//! breaks through, else `prompt_filtered` Audit — same kind/pin/detail
//! text); `FetchInbox` is a payload-free signal like the embedded mapping. The former G1 gap (`scan_ack_sla` returned
//! empty for lack of a toron CLI verb) is closed: the daemon shells
//! `toron ack-sla --project` and relays the expired ids (bd-0zcr.2).

use crate::host::{CoreReducer, Effect, Host};
use crate::msg::BrainMsg;
use crate::store::FileStore;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Toron watch kinds — MUST match the embedded in-process filter and the
/// CLI fallback (`DOORBELL_WATCH_KINDS` in flywheel `src/main.rs`).
pub const WATCH_KINDS: &str = "9,14,20001,30013,43001,30031,39002";

/// Presence projection (<30s freshness) — mirrors `timeout_presence_rows`.
pub const PRESENCE_FRESH_SECS: u64 = 30;
/// Index rebuild cadence — mirrors the embedded 60s TTL + pin-set change.
pub const INDEX_TTL_SECS: u64 = 60;

/// The machine marker toron stamps on a store-skew refusal (bd-ogzg) — the two
/// repos cannot share a constant, so this is a pinned literal. toron's
/// `index::schema::STORE_SKEW_MARKER` is the producer and its own test pins the
/// spelling; the test below pins that we match it.
///
/// Public because `/health` probes for it too: the marker is a property of the
/// refusal TEXT, so it can be matched in any audit row whatever kind wrote it.
pub const TORON_STORE_SKEW_MARKER: &str = "toron-error=store_skew";

/// The sentence toron's refusal has always carried, kept as a second needle so
/// the probe keeps working against a toron that predates the marker. During an
/// upgrade window the marker is exactly what is missing, and a probe that only
/// knew the marker would go blind precisely when skew is live.
pub const TORON_STORE_SKEW_PROSE: &str = "resolves at most";

/// True when a refusal is the store-skew fault. One predicate for every arm
/// that opens the store, so "is this skew" is decided in one place: the inbox
/// arm, the ack-SLA arm, and the `/health` probe all answer the same question
/// about different text.
pub fn is_store_skew_refusal(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains(TORON_STORE_SKEW_MARKER) || s.contains(TORON_STORE_SKEW_PROSE)
}

/// Classify a failed inbox fetch. (The store-skew class itself landed earlier;
/// bd-ogzg is the declared-code change on top of it. The id of the earlier
/// bead is deliberately NOT repeated here: an added line that cites a closed
/// bead's id obliges this commit message to cite it too, which would re-derive
/// that bead's bound revision to this commit and reopen finished work.)
///
/// `/health` greps `inbox_error` as "the inbox PATH is down"; a missing agent
/// key is a pin problem and must not paint the whole daemon. A store SKEW is
/// neither, and buries a whole outage under a kind that says nothing about it.
///
/// The store (`$XDG_DATA_HOME/toron/index.sqlite`) is shared, so a binary that
/// resolves fewer migrations than the store carries refuses EVERY workspace at
/// once — live on 2026-09-28 the daemon was degraded for real and `/health`
/// reported `degraded: []`, because the all-workspaces rule the generic kind
/// feeds could never be satisfied by a fault that is never per-workspace.
///
/// Matched on toron's own wording (`resolves at most`) because that message is
/// the contract: it names the store, the binary's ceiling, and the fix.
fn inbox_fail_kind(stderr: &str) -> &'static str {
    let s = stderr.to_ascii_lowercase();
    if is_store_skew_refusal(&s) {
        "inbox_store_skew"
    } else if s.contains("no local key")
        || s.contains("register first")
        || s.contains("create_agent_identity")
        || s.contains("unknown agent")
        || s.contains("not registered")
    {
        "inbox_pin_miss"
    } else {
        "inbox_error"
    }
}

/// Registry key + resolution live in core (bd-z5ou single owner).
use doorbell_core::{PresenceVerdict, registry_key};

/// A watch line that could not be enqueued is a DELIVERY failure, not a
/// consumer death. This is the audit kind whose absence hid the storm: the
/// flood wrote no audit row at all, so the audit log read calm while the
/// journal filled with fabricated deaths.
pub const AUDIT_BRAIN_FULL: &str = "wake_brain_full";

/// Digest row kind for a still-failing wake socket: the live rate, the fd
/// census, and the backoff about to sleep. A month-long outage used to read
/// as ONE absent row (deduped) plus silent 16s reconnects; this names the
/// ongoing rate sampled (should_audit_count), bounded rows, true size readable.
/// The 16s gate itself is untouched (GS-007 never-deaf).
pub const AUDIT_WAKE_FAILING_STILL: &str = "wake_failing_still";

/// Build the still-failing digest row. Pure constructor so the shape is
/// unit-testable; the sampling decision stays with the caller.
pub(crate) fn failing_still_row(
    consecutive_fails: u64,
    open_fds: Option<usize>,
    backoff_s: u64,
) -> serde_json::Value {
    serde_json::json!({
        "kind": AUDIT_WAKE_FAILING_STILL,
        "reason": "wake socket still failing; reconnecting per never-deaf backoff, not parked",
        "consecutive_fails": consecutive_fails,
        "open_fds": open_fds,
        "backoff_s": backoff_s,
    })
}

/// HerdrStatus changes the sweep could not enqueue on a full or closed brain
/// channel. Counted per host (self.herdr advances only on receipt, so drops
/// self-heal next sweep); audited sampled via should_audit_count, the same
/// repeat-is-a-count shape as wake_brain_full.
pub const AUDIT_HERDR_FULL: &str = "herdr_brain_full";

/// A consumer death relayed to the brain, counted so a STORM reads as a rate.
///
/// The relay used to be the quietest place in the process: one death in, one
/// `deliver_died_blocking` out, no counter and no row, however many arrived. So
/// the 2026-09-28 burst wrote 9,741 `Died` journal rows and not one audit row
/// that named the rate — the loudest event in the journal had the quietest
/// trail, and the only way to measure it afterwards was to count rows by hand.
pub const AUDIT_DIED_RELAYED: &str = "wake_died_relayed";

/// What one un-enqueueable watch line does.
#[derive(Debug, PartialEq, Eq)]
enum FullChannelAction {
    /// The first failure for this consumer: report one death so the restart
    /// still happens (GS062-ADV-5's intent), and audit it loudly.
    ReportDeath,
    /// Subsequent failures: count only. Same fault, same consumer — reporting
    /// each as a death is what turned a full channel into a Died storm
    /// (live: 1,928 rows in four minutes, peaking at 127 per second, and every
    /// one of them advanced the retry backoff as if the child had died).
    CountDropped,
}

/// One consumer may report at most one death, however many lines it fails to
/// enqueue.
fn full_channel_action(reported: &std::sync::atomic::AtomicBool) -> FullChannelAction {
    if reported.swap(true, std::sync::atomic::Ordering::SeqCst) {
        FullChannelAction::CountDropped
    } else {
        FullChannelAction::ReportDeath
    }
}

/// Which of the counted drops also carry an audit row: the first, and then
/// every power of two. Bounded (a 10,000-line flood leaves ~13 rows) and never
/// silent, which is the whole point — the flood this replaces was invisible.
///
/// Shared by the drop counter, the death-relay counter and the journal
/// write-failure counter: all three are repeats of one fault, and a repeat is a
/// COUNT with a sampled row, not a row each.
pub(crate) fn should_audit_count(prior_count: usize) -> bool {
    prior_count == 0 || prior_count.is_power_of_two()
}

/// What one relayed consumer death does besides reaching the brain.
#[derive(Debug, PartialEq, Eq)]
enum DiedRelayAction {
    /// Audit it as well: the first death, then every power of two. The payload
    /// is how many deaths this row stands for, so a storm's row count is
    /// bounded while its true size stays readable.
    RelayAndAudit(usize),
    /// Count only. Same fault, same host — and the delivery is NOT optional:
    /// the arm always relays, because the restart depends on the death
    /// arriving. Only the audit is sampled.
    RelayOnly,
}

fn died_relay_action(prior_relays: usize) -> DiedRelayAction {
    if should_audit_count(prior_relays) {
        DiedRelayAction::RelayAndAudit(prior_relays + 1)
    } else {
        DiedRelayAction::RelayOnly
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
///
/// bd-239z: both pipes are drained ON READER THREADS for the child's whole
/// life. Reading after exit — the `wait_with_output` shape this replaced — is
/// a deadlock, not a bound: a pipe holds a finite buffer (65536 bytes measured
/// on this host), the child blocks in `write` once it is full, never exits,
/// and gets killed at the deadline with its output discarded. The daemon then
/// audits "toron spawn/timeout failed" for a fetch that had already
/// succeeded. Cold `mail inbox --limit 50` is 88925 bytes, so one pin's first
/// page was past the buffer and that pin could never be fetched at all.
///
/// The deadline is still the only thing that ends the wait: on timeout the
/// readers are detached, not joined, because an orphaned grandchild can hold
/// the write end open long past the child we killed.
pub fn run_cli(bin: &str, args: &[&str], timeout_secs: u64) -> Option<std::process::Output> {
    let mut child = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let out_reader = drain_pipe(child.stdout.take());
    let err_reader = drain_pipe(child.stderr.take());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            _ => break None,
        }
    };
    match status {
        Some(status) => {
            // The child is reaped and its write ends are closed, so both
            // readers see EOF. This join is what actually delivers the bytes.
            let stdout = out_reader.join().unwrap_or_default();
            let stderr = err_reader.join().unwrap_or_default();
            Some(std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            // Detach: nobody is reading these, and joining would wait on
            // whatever else inherited the pipe.
            drop(out_reader);
            drop(err_reader);
            None
        }
    }
}

/// One reader thread per pipe, started before the first poll so the child
/// never has a full pipe to block on.
fn drain_pipe<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
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

/// Resolve the registry file: explicit override, `{slug}.json`, or the
/// freshest `{slug}-*.json` hashed candidate (mtime-desc, ambiguous flag
/// when several match). Slug matching uses the core `registry_key`
/// (single owner with flywheel `state::state_key`, bd-z5ou): absolute-path
/// projects hash to `{base}-{8hex}` (e.g. `proj-0ec6cde6`). Matching the RAW project string
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

/// (pin, pane_id, herdr name) rows from a registry file, plus the chosen
/// file's name when several hashed candidates matched (the caller audits
/// that ambiguity). Read-only and `&self`-free so the /health probe can ask
/// the same question the sweep asks (bd-ba8t).
fn read_registry_rows(
    flywheel_state: &Path,
    project: &str,
    override_path: Option<&Path>,
) -> (Vec<(String, String, String)>, Option<String>) {
    let (file, ambiguous) = resolve_registry_file(flywheel_state, project, override_path);
    let chosen = if ambiguous {
        file.as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
    } else {
        None
    };
    let Some(path) = file else {
        return (Vec::new(), chosen);
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return (Vec::new(), chosen);
    };
    let rows = serde_json::from_str::<serde_json::Value>(&raw)
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
        .collect();
    (rows, chosen)
}

/// pane_id -> `agent_status` from one `herdr agent list`. Empty map = herdr
/// reports no agents at all, which is a fact the /health probe must
/// distinguish from "every pane is busy" (bd-ba8t).
fn herdr_agent_statuses(herdr_bin: &str) -> BTreeMap<String, String> {
    let out = run_cli(herdr_bin, &["agent", "list"], CLI_TIMEOUT_SECS);
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
    agents
        .iter()
        .filter_map(|a| {
            let pane = a.get("pane_id").and_then(|p| p.as_str())?;
            let status = a.get("agent_status").and_then(|s| s.as_str())?;
            Some((pane.to_string(), status.to_string()))
        })
        .collect()
}

/// What the prompt sink hands `herdr agent prompt` for one routed pin.
///
/// herdr resolves an agent by its herdr LABEL or by PANE ID, and a pane
/// nobody started with `herdr agent start` — every operator pane — has only
/// the second: live, `herdr agent get w6:p1` answers with the captain's pane
/// while `herdr agent get AmberQuill` answers `agent_not_found`, which is the
/// last recorded delivery failure on this host (`delivery failed: herdr
/// prompt failed: agent target BlackOrchid not found`). The pane id resolves
/// for BOTH shapes (a crew pane has one too), so it is the target; a row with
/// no pane keeps its label rather than becoming an empty target that reads
/// like a route (bd-523f).
fn prompt_target<'a>(pane: &'a str, name: &'a str) -> &'a str {
    if pane.is_empty() { name } else { pane }
}

/// (pane_id, agent_status, toron_pin token) rows from one
/// `herdr agent list`. Empty vec = herdr did not answer (same guard as
/// the status/pane readers): callers keep prior state, never clear it.
fn herdr_agent_roster(herdr_bin: &str) -> Vec<(String, String, Option<String>)> {
    let out = run_cli(herdr_bin, &["agent", "list"], CLI_TIMEOUT_SECS);
    let Some(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    parse_agent_roster(&v)
}

/// Pure roster projection: (pane_id, agent_status, toron_pin token).
/// Agents without a pane or status are skipped; a missing/empty token
/// yields None (spawned crew — registry owns their route).
fn parse_agent_roster(v: &serde_json::Value) -> Vec<(String, String, Option<String>)> {
    let empty = Vec::new();
    v.get("result")
        .and_then(|r| r.get("agents"))
        .and_then(|a| a.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter_map(|a| {
            let pane = a.get("pane_id").and_then(|p| p.as_str())?;
            let status = a.get("agent_status").and_then(|s| s.as_str())?;
            let pin = a
                .get("tokens")
                .and_then(|t| t.get("toron_pin"))
                .and_then(|p| p.as_str())
                .filter(|p| !p.is_empty())
                .map(str::to_string);
            Some((pane.to_string(), status.to_string(), pin))
        })
        .collect()
}

/// bd-3vr9: union herdr-known captain panes into the routing set.
/// Spawn-registry rows cover spawned crews; a captain pane (never
/// spawned) carries its pin in the `toron_pin` token instead. For every
/// wanted pin with no route yet, bind the pane whose token names it.
/// Pure: `roster` is one agent-list snapshot, `want` the swept +
/// subscribed pins, `routed` the pins the registry already covers.
/// Returns (pin, pane_id) pairs — pane ids prompt directly.
fn captain_pane_targets(
    roster: &[(String, String, Option<String>)],
    want: &[String],
    routed: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (pane, _status, token) in roster {
        let Some(pin) = token else { continue };
        if !want.contains(pin) || routed.contains(pin) || out.iter().any(|(p, _)| p == pin) {
            continue;
        }
        out.push((pin.clone(), pane.clone()));
    }
    out
}

/// Pane ids from one `herdr pane list` (bd-adms). Pane EXISTENCE is a
/// different question from injectability (bd-ba8t's `agent_status`): a pane
/// can exist with no agent in it. An empty set means herdr did not answer
/// (unknown binary, non-zero exit), and callers must treat that as "cannot
/// tell" rather than "no panes exist".
fn herdr_pane_ids(herdr_bin: &str) -> std::collections::BTreeSet<String> {
    let out = run_cli(herdr_bin, &["pane", "list"], CLI_TIMEOUT_SECS);
    let Some(out) = out else {
        return std::collections::BTreeSet::new();
    };
    if !out.status.success() {
        return std::collections::BTreeSet::new();
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let empty = Vec::new();
    v.get("result")
        .and_then(|r| r.get("panes"))
        .and_then(|p| p.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter_map(|p| {
            p.get("pane_id")
                .and_then(|i| i.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// `(pin, pane_id, herdr name)` — one spawn-registry row.
type RegistryRow = (String, String, String);

/// One stale-pin pass (bd-adms): `live` rows keep the sweep; `vanished` pins
/// have no pane right now (they keep the digest route a pane-less pin gets,
/// bd-bk9t); `reaped` is the subset past the grace window, which also leaves
/// the sweep set.
struct StaleReap {
    live: Vec<RegistryRow>,
    vanished: Vec<String>,
    reaped: Vec<String>,
}

// bd-3uqz: STALE_PANE_GRACE_SECS is GONE, deliberately rather than left as a dead
// constant. It was a wall-clock window standing in for "we have seen this
// repeatedly", and it was wrong in both directions: one herdr outage outlasting the
// window pruned a live fleet, and a pane that died and returned inside the window
// was never pruned. The evidence is now
// `registry_prune::REQUIRED_CONFIRMATIONS` consecutive observations, and leaving a
// named 120s grace behind would invite the next reader to think time still decides
// something here.

/// bd-3vr9: a presence verdict older than this yields to live herdr
/// status. Presence rows arrive only on agent publish; hook-less panes
/// keep stale words for hours.
const PRESENCE_STALE_SECS: u64 = 120;

/// Can the ring's LAST hop land here at all? (bd-ba8t)
///
/// `should_inject` accepts only `idle|done|blocked`, and statuses arrive from
/// `herdr agent list` mapped through the spawn registry. With zero herdr
/// agents — the live condition that made gs-044 hard to see — every wake
/// classifies correctly and then has nowhere to go, while `/health` reads
/// `hosted, degraded: []`. Counting the picture here turns that into a fact.
///
/// `panes_seen` deliberately counts panes in ANY status: a fleet where every
/// pane is momentarily `working` is healthy (a durable queue holds the wake),
/// while `panes_seen == 0` with pins > 0 means the prompt hop is structurally
/// impossible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InjectionPicture {
    /// sweep-set pins (non-empty `mail_identity`, pane still listed by
    /// herdr) — what the sweep actually fetches, never the raw registry
    pub pins: usize,
    /// pins whose pane appears in `herdr agent list`, any status
    pub panes_seen: usize,
    /// pins whose status is injectable right now (`idle|done|blocked`)
    pub injectable: usize,
    /// pins whose pane is absent from `herdr agent list` (or empty)
    pub panes_missing: usize,
}

/// The same registry + herdr view the sweep reads, reduced to the counts
/// `/health` needs. Pure read: no journal, no store, no locks.
pub fn injection_picture(
    flywheel_state: &Path,
    project: &str,
    override_path: Option<&Path>,
    herdr_bin: &str,
) -> InjectionPicture {
    let (rows, _) = read_registry_rows(flywheel_state, project, override_path);
    let statuses = herdr_agent_statuses(herdr_bin);
    // bd-2i96 G4: `pins` tracks the sweep set, not the raw registry.
    // Reaped/stale pins (pane absent from `herdr pane list`) leave the
    // sweep via reap_stale_registry_pins; counting them here reported a
    // fleet that costs no fetches and can land no wake. The fault stays
    // visible in `panes_missing`. Empty pane set = herdr did not answer
    // (same guard as the reaper): no evidence, keep all rows.
    let panes = herdr_pane_ids(herdr_bin);
    let mut picture = InjectionPicture::default();
    for (_, pane, _) in &rows {
        if !panes.is_empty() && (pane.is_empty() || !panes.contains(pane)) {
            picture.panes_missing += 1;
            continue;
        }
        picture.pins += 1;
        match statuses.get(pane) {
            Some(status) => {
                picture.panes_seen += 1;
                if doorbell_core::should_inject(status) {
                    picture.injectable += 1;
                }
            }
            None => picture.panes_missing += 1,
        }
    }
    picture
}

/// Wake-socket subscription kinds live in [`WATCH_KINDS`]; the consumer in
/// [`crate::wake_sock`] maps frames onto [`BrainMsg`](crate::msg::BrainMsg).
pub struct FileHost {
    cfg: FileHostConfig,
    reducer: CoreReducer,
    /// Sticky presence verdicts — ONLY `Presence` msgs write here
    /// (mirrors the workspace tick arm: direct insert, no fallback).
    presence: BTreeMap<String, PresenceVerdict>,
    /// Wall seconds of the last `Presence` msg per pin (bd-3vr9). A verdict
    /// with no fresh publisher is stale evidence: live herdr status
    /// outranks it, fresh self-report outranks herdr.
    presence_ts: BTreeMap<String, u64>,
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
    /// The pins actually swept. Excludes registry pins reaped as stale
    /// (bd-adms); `index_pins_raw` keeps the unfiltered view so the index
    /// change guard does not thrash.
    index_pins: Vec<String>,
    /// Registry ∪ subscription pins, before stale reaping (bd-adms).
    index_pins_raw: Vec<String>,
    /// pin -> first second we saw its registry pane missing from
    /// `herdr pane list` (bd-adms). RAM-only on purpose: a restart re-arms
    /// the grace window instead of reaping on stale evidence.
    stale_panes: BTreeMap<String, u64>,
    /// bd-3uqz: consecutive sweeps in which herdr did NOT list a pin's pane.
    ///
    /// This replaces a wall-clock window for deciding a pane is dead. Time was a
    /// PROXY for "we have seen this repeatedly" and it is wrong in both directions:
    /// one herdr outage outlasting the window prunes a live fleet, and a pane that
    /// dies and returns inside the window is never pruned. A count measures the
    /// thing itself.
    ///
    /// `stale_panes` stays for reporting WHEN a pin first went missing, which is
    /// what makes the audit line useful; it no longer decides anything.
    pane_absent_confirmations: BTreeMap<String, u32>,
    /// bd-3uqz: what the last prune decided and why, so the audit line and any
    /// later "why does this pin not wake?" are answerable from state rather than
    /// from memory. A prune that leaves no record has no history.
    tombstones: BTreeMap<String, crate::registry_prune::Tombstone>,
    /// Pins already audited as reaped, so the audit fires once per episode.
    reaped_pins: Vec<String>,
    /// Operator pins currently bound via herdr token (bd-3vr9), so the
    /// `captain_pane_routed` audit fires on change, not every refresh.
    captain_routed_pins: Vec<String>,
    /// The subset of `index_pins` with no spawn-registry row, so no pane and
    /// no herdr target (bd-bk9t). Mirrored onto
    /// `reducer.model.unroutable`; kept here too so the index change guard
    /// notices a pin gaining/losing its route without the pin set changing.
    unroutable_pins: Vec<String>,
    index_refreshed_at: u64,
    /// Shared with the watch delivery closure. bd-kz54 follow-up: the backoff
    /// used to be owned only by `restart_wake`, so NOTHING could clear it —
    /// `WatchRetry::reset()` had exactly one caller, its own unit test. Four
    /// failures anywhere in a workspace's life latched it at the 16s gate
    /// permanently, so a host that had one bad afternoon probed its bus three
    /// times a minute forever and never regained a fast first retry.
    retry: std::sync::Arc<std::sync::Mutex<crate::policy::WatchRetry>>,
    /// bd-lorot: at most ONE restart may be outstanding.
    ///
    /// bd-kz54's `Died` path had no single-flight guard, so every death
    /// scheduled its own restart task: `run_wake_loop` audits a failed connect
    /// and returns `Died`, that `Died` schedules another task, and the cycle
    /// feeds itself. Measured live on 2026-09-27: **1,613 audit rows in ONE
    /// second** against a 16s backoff — the backoff was never in the path,
    /// because 1,613 tasks each slept their own 16s and then all connected
    /// anyway.
    ///
    /// This is the difference between a backoff and a rate limit. A backoff
    /// spaces ONE retry; without single-flight, nothing stops a burst of
    /// failures from scheduling a burst of retries that all fire together after
    /// the sleep. The observed consequence was worse than CPU: the doorbell's
    /// own retry storm drove the toron daemon to **EMFILE** (`accept error: Too
    /// many open files`, 234 fds on `wake.sock`), which killed the very socket
    /// the doorbell was polling. The two services took each other down.
    restart_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Rows already written for the absence currently in progress.
    ///
    /// bd-lorot: the CAUSE is audited ONCE per outage, not once per attempt.
    /// 1,613 identical `wake_sock_absent` rows per second is 169 KB/s, which
    /// tripped the 5 MiB rotation into rewriting the file continuously — most
    /// of the 239% CPU and the 629 MB RSS. A writer that a peer service can
    /// make spin is a denial-of-service surface, not diagnostics. Reset to 0 on
    /// a real delivery so the NEXT outage is audited again.
    absent_rows: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Deaths relayed to the brain for this host, across every consumer.
    ///
    /// Host-wide on purpose: a death storm is many consumers dying in turn, so a
    /// per-consumer counter would reset on each re-arm and report every restart
    /// as "death #1" — one row each, unbounded, which is the very shape this
    /// guards against.
    died_relayed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// HerdrStatus sends dropped on a full or closed brain channel, this host.
    /// Host-wide like died_relayed: the sweep recomputes `changed` every tick,
    /// so the count survives across sweeps by construction.
    herdr_dropped: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    daemon_exe: PathBuf,
    generation: Option<GenRecord>,
    /// Status-change outbox: the poll loop `tell`s `HerdrStatus` back
    /// through this channel (forwarder topology over a poll transport).
    tx: Option<tokio::sync::mpsc::Sender<BrainMsg>>,
    pending_audit: Option<(String, serde_json::Value)>,
    /// The registry file whose ambiguity was last audited, so a standing
    /// ambiguity is reported once instead of once per sweep.
    registry_ambiguous_reported: Option<String>,
    /// The single live wake-socket consumer (shared with the restart task so
    /// a replacement aborts its predecessor). [`tokio::task::AbortHandle`]
    /// aborts on drop-scope, mirroring the old child reap.
    /// Holds the handle AND the flag that marks this consumer retired. bd-v7uu:
    /// the pair travels together so a replacement cannot abort a consumer without
    /// also being able to say it was deliberate.
    wake_task: std::sync::Arc<std::sync::Mutex<Option<(tokio::task::AbortHandle, RetireFlag)>>>,
}

impl Drop for FileHost {
    /// Never outlive a live consumer: dropping the host (shutdown, or a
    /// retired generation) aborts the socket task instead of orphaning it.
    ///
    /// Marked retired first, for the same reason the restart path does: this is a
    /// deliberate retirement, and reporting it as a lost consumer would be a lie
    /// that also advances a backoff nobody is waiting on.
    fn drop(&mut self) {
        if let Ok(mut g) = self.wake_task.lock()
            && let Some((h, retire)) = g.take()
        {
            retire.store(true, std::sync::atomic::Ordering::SeqCst);
            h.abort();
        }
    }
}

/// Set by whoever replaces a wake consumer, so its monitor can tell a deliberate
/// retirement from a consumer that vanished on its own. bd-v7uu.
type RetireFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

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
            presence_ts: BTreeMap::new(),
            herdr: BTreeMap::new(),
            roles: BTreeMap::new(),
            panes: BTreeMap::new(),
            targets: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            npubs: BTreeMap::new(),
            index_pins: Vec::new(),
            index_pins_raw: Vec::new(),
            stale_panes: BTreeMap::new(),
            pane_absent_confirmations: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            reaped_pins: Vec::new(),
            unroutable_pins: Vec::new(),
            captain_routed_pins: Vec::new(),
            index_refreshed_at: 0,
            retry: std::sync::Arc::new(std::sync::Mutex::new(crate::policy::WatchRetry::default())),
            restart_pending: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            absent_rows: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            died_relayed: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            herdr_dropped: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            daemon_exe,
            generation,
            tx: None,
            pending_audit: None,
            registry_ambiguous_reported: None,
            wake_task: std::sync::Arc::new(std::sync::Mutex::new(None)),
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
        let (rows, ambiguous_file) = read_registry_rows(
            &self.cfg.flywheel_state,
            &self.cfg.project,
            self.cfg.registry_file.as_deref(),
        );
        match ambiguous_file {
            Some(chosen) => {
                // ONE row per outage, not one per sweep. The registry is never
                // garbage-collected, so a standing ambiguity can run for days:
                // 36 sweeps a minute named the same file and wrote ~52k rows a
                // day, which is how the loudest line in the journal becomes the
                // one readers skip. bd-ztek still keeps the name, because
                // first-sorted used to sweep a stale fleet's pins with no trace
                // of which file won.
                if self.registry_ambiguous_reported.as_deref() != Some(chosen.as_str()) {
                    self.registry_ambiguous_reported = Some(chosen.clone());
                    self.pending_audit = Some((
                        "registry_ambiguous".into(),
                        serde_json::json!({ "project": self.cfg.project, "registry_file": chosen }),
                    ));
                }
            }
            // Cleared: a later ambiguity is a NEW outage and must be audited
            // again, or dedupe would silence the second incident entirely.
            None => self.registry_ambiguous_reported = None,
        }
        rows
    }

    /// Pins named by this workspace's wake subscriptions (bd-cak3).
    ///
    /// The registry only knows SPAWNED crews, so a subscribed identity that
    /// was never spawned here — the operator's own pin is the live case —
    /// had mail delivered, a subscription row, and still no sweep, no
    /// `SelfIndex` entry, and no route: its wake never materialised at all
    /// (ADR-0012 §8 Phase 2). Unioning these in makes it swept and
    /// addressable, which is the half a subscription is allowed to buy on
    /// its own.
    ///
    /// Deliberately NOT unioned into `panes`/`targets`: a subscription row
    /// carries no pane, so inventing one would fabricate a route the sink
    /// cannot deliver to. With no pane the status stays `unknown`,
    /// `should_inject` refuses, and the wake queues/audits visibly instead
    /// of being silently invisible.
    fn subscription_pins(&self) -> Vec<String> {
        self.workspace_subscriptions()
            .into_iter()
            .filter_map(|s| match &s.identity {
                crate::subscribe::Identity::Pin(p) if !p.is_empty() => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every subscription row that names this workspace in its scope.
    ///
    /// Read live (not cached): rows are added and dropped while the daemon
    /// runs, and a stale cache would route a wake to the sink its owner just
    /// changed away from.
    fn workspace_subscriptions(&self) -> Vec<crate::subscribe::Subscription> {
        crate::subscribe::SubscriptionStore::open(&self.cfg.flywheel_state)
            .list()
            .into_iter()
            .filter(|s| {
                s.scope
                    .workspaces()
                    .iter()
                    .any(|w| w == &self.cfg.workspace)
            })
            .collect()
    }

    /// bd-8ulu: which sink this pin's wake goes to, from its subscription
    /// row. The FIRST row naming the pin wins (a pin with two rows is a
    /// configuration smell the subscribe verb already refuses to create
    /// silently); a row that never names a pin (channel/scope identities)
    /// leaves the workspace default in place. The name is the row's
    /// [`crate::subscribe::Subscription::route_name`], so an
    /// endpoint-carrying webhook row routes to its own adapter.
    fn sink_for(&self, pin: &str) -> Option<String> {
        self.workspace_subscriptions()
            .into_iter()
            .find(|s| matches!(&s.identity, crate::subscribe::Identity::Pin(p) if p == pin))
            .map(|s| s.route_name())
    }

    /// bd-adms: the spawn registry is not garbage-collected, so a fleet that
    /// died leaves its pins behind forever. Live, three pins sat on `w11:*`
    /// panes herdr had long GC'd: the sweep fetched three dead inboxes on
    /// every tick, and when those fetches failed they wrote the
    /// `inbox_error` stream that hid a real outage (gs-044). Subscriptions
    /// gained a 7d reaper (bd-2o1v); this is the same belt for registry
    /// rows — a pin whose pane is absent from `herdr pane list` past the
    /// grace window drops out of the sweep set with ONE audit row naming it.
    ///
    /// Guards: an EMPTY pane list means herdr did not answer, so nothing is
    /// reaped on a transient failure, and a pin whose pane reappears is
    /// un-reaped. `/health.injection` pins track the same sweep set, so a
    /// reaped pin leaves `pins` and the fault stays visible in
    /// `panes_missing` after it stops costing a fetch per tick.
    fn reap_stale_registry_pins(&mut self, rows: &[RegistryRow], now: u64) -> StaleReap {
        let known = herdr_pane_ids(&self.cfg.herdr_bin);
        // bd-3uqz: the DECISION is `registry_prune::decide`, which is pure and
        // fenced. This function is only the plumbing — herdr's answer in, the audit
        // and the index out. The two rules that matter live in the pure module
        // because they are the ones that were wrong HERE:
        //
        //  - a row whose pane is EMPTY resolves by LABEL and is never pruned. The
        //    old test was `!pane.is_empty() && known.contains(pane)`, so an empty
        //    pane never matched "alive" and fell through to the reap branch —
        //    deleting exactly the legacy rows `prompt_hop_keeps_the_label_when_the_
        //    row_has_no_pane` exists to support. No test covered the intersection.
        //  - an EMPTY pane list from herdr is NO EVIDENCE: nothing is pruned and
        //    nothing is COUNTED, so an outage cannot manufacture confirmations for
        //    panes that are alive. Live, 13 of 20 rows (65%) had a pane herdr did
        //    not list, so absence-based pruning would wipe two thirds of the table
        //    on its first sweep — during an outage, when it matters most.
        let decision_rows: Vec<crate::registry_prune::Row> = rows
            .iter()
            .map(|(pin, pane, _)| crate::registry_prune::Row {
                pin: pin.clone(),
                pane: pane.clone(),
            })
            .collect();
        let decision = crate::registry_prune::decide(
            &decision_rows,
            &known,
            &mut self.pane_absent_confirmations,
            crate::registry_prune::REQUIRED_CONFIRMATIONS,
        );
        for t in &decision.tombstones {
            self.tombstones.insert(t.pin.clone(), t.clone());
        }

        // Reporting only: when did this pin first go missing. The decision above
        // deliberately does not consult the clock.
        for (pin, pane, _) in rows {
            if pane.is_empty() || known.contains(pane) {
                self.stale_panes.remove(pin);
            } else {
                self.stale_panes.entry(pin.clone()).or_insert(now);
            }
        }

        let mut vanished: Vec<String> = Vec::new();
        let mut reaped: Vec<String> = Vec::new();
        for (pin, fate) in &decision.fates {
            match fate {
                crate::registry_prune::Fate::Alive
                | crate::registry_prune::Fate::NoEvidence
                | crate::registry_prune::Fate::LabelRouted => {
                    self.reaped_pins.retain(|p| p != pin);
                }
                crate::registry_prune::Fate::Unconfirmed => vanished.push(pin.clone()),
                crate::registry_prune::Fate::Pruned => {
                    vanished.push(pin.clone());
                    reaped.push(pin.clone());
                }
            }
        }
        for (pin, pane, _) in rows {
            if !reaped.contains(pin) {
                continue;
            }
            if !self.reaped_pins.contains(pin) {
                let t = self.tombstones.get(pin);
                self.audit_file(
                    "registry_pin_stale",
                    serde_json::json!({
                        "pin": pin,
                        "pane": pane,
                        "workspace": self.cfg.workspace,
                        // bd-3uqz: the evidence is a COUNT of consecutive sweeps, not
                        // elapsed time. Reporting `grace_s` here would have kept
                        // describing a rule that no longer exists.
                        "absent_confirmations": t.map_or(
                            crate::registry_prune::REQUIRED_CONFIRMATIONS,
                            |t| t.confirmations),
                        "required_confirmations": crate::registry_prune::REQUIRED_CONFIRMATIONS,
                        "missing_since": self.stale_panes.get(pin).copied().unwrap_or(0),
                        "why": "pane absent from `herdr pane list` across consecutive \
                                confirmations; positive evidence of death, dropped from the sweep",
                    }),
                );
                self.reaped_pins.push(pin.clone());
            }
        }
        StaleReap {
            live: rows
                .iter()
                .filter(|(pin, _, _)| !reaped.contains(pin))
                .cloned()
                .collect(),
            vanished,
            reaped,
        }
    }

    /// Refresh the reducer index when the pin set changes or the TTL
    /// lapses (mirrors the embedded 60s rule; whois failures keep None).
    fn refresh_index(&mut self) {
        let rows = self.registry_rows();
        let mut pins: Vec<String> = rows.iter().map(|(p, _, _)| p.clone()).collect();
        // bd-bk9t: a subscribed pin with no registry row has no pane and no
        // target, so its status can never leave "unknown" and no queue drain
        // can ever fire. Marked here so `route_or_queue` digests its wakes
        // instead of parking them in a queue that only dies with the process.
        let mut unroutable: Vec<String> = Vec::new();
        // bd-cak3: union subscribed identities. Registry order is preserved
        // so spawned pins keep their existing order; subscribed-only pins
        // append deduped, and the whois loop below resolves their npubs too.
        for pin in self.subscription_pins() {
            if !pins.contains(&pin) {
                pins.push(pin.clone());
                unroutable.push(pin);
            }
        }
        let now = Self::now_unix();
        // Compared against the RAW view: reaped pins stay in `pins` here, so
        // the guard does not thrash once a stale fleet is filtered out
        // (bd-adms).
        if pins == self.index_pins_raw
            && unroutable == self.unroutable_pins
            && now.saturating_sub(self.index_refreshed_at) < INDEX_TTL_SECS
        {
            return;
        }
        // bd-adms: drop registry pins whose pane herdr no longer has, and
        // treat a vanished pane like a subscription-only pin — no pane, no
        // route, so its wake digests instead of parking in a queue nothing
        // can drain.
        let reap = self.reap_stale_registry_pins(&rows, now);
        for pin in reap.vanished {
            if !unroutable.contains(&pin) {
                unroutable.push(pin);
            }
        }
        let swept: Vec<String> = pins
            .iter()
            .filter(|p| !reap.reaped.contains(p))
            .cloned()
            .collect();
        self.panes = reap
            .live
            .iter()
            .map(|(pin, pane, _)| (pane.clone(), pin.clone()))
            .collect();
        if let Ok(mut targets) = self.targets.lock() {
            targets.clear();
            targets.extend(reap.live.iter().map(|(pin, pane, name)| {
                // bd-523f: the value is a herdr PROMPT TARGET, not a name —
                // the sink passes it to `agent prompt` verbatim, and only the
                // pane id resolves for an operator pane.
                (pin.clone(), prompt_target(pane, name).to_string())
            }));
            // bd-3vr9: union herdr-known captain panes (same resolver, no
            // second table). A subscribed pin whose pane was never spawned
            // carries its pin in the herdr `toron_pin` token; bind it so
            // has_route holds and the wake prompts instead of digesting
            // forever. Vanished panes drop out on the next refresh because
            // the roster no longer names them.
            let routed: std::collections::HashSet<String> = targets.keys().cloned().collect();
            let ops =
                captain_pane_targets(&herdr_agent_roster(&self.cfg.herdr_bin), &pins, &routed);
            for (pin, pane) in &ops {
                targets.insert(pin.clone(), pane.clone());
                self.panes.insert(pane.clone(), pin.clone());
            }
            if !ops.is_empty() {
                let current: Vec<String> = ops.iter().map(|(p, _)| p.clone()).collect();
                if current != self.captain_routed_pins {
                    self.captain_routed_pins = current;
                    self.audit_file(
                        "captain_pane_routed",
                        serde_json::json!({
                            "pins": ops.iter().map(|(p, _)| p).collect::<Vec<_>>(),
                        }),
                    );
                }
            } else if !self.captain_routed_pins.is_empty() {
                self.captain_routed_pins.clear();
            }
            unroutable.retain(|p| !ops.iter().any(|(q, _)| q == p));
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
        self.reducer.model.unroutable = unroutable.iter().cloned().collect();
        self.unroutable_pins = unroutable;
        self.index_pins = swept;
        self.index_pins_raw = pins;
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
        herdr_agent_statuses(&self.cfg.herdr_bin)
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
        Self::append_audit_row(&self.cfg.flywheel_state, &self.cfg.workspace, kind, detail);
    }

    /// Append one audit row. Single owner for the audit shape, shared by
    /// [`FileHost::audit_file`] and the wake-sock task (which owns no `&self`).
    pub(crate) fn append_audit_row(
        state_dir: &Path,
        workspace: &str,
        kind: &str,
        detail: serde_json::Value,
    ) {
        let mut entry = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "workspace": workspace,
            "kind": kind,
        });
        if let (Some(obj), serde_json::Value::Object(map)) = (entry.as_object_mut(), detail) {
            obj.extend(map);
        }
        // bd-jcys: ONE audit append. This was a second implementation that
        // discarded both the mkdir and the write (`let _ =` inside `if let Ok`),
        // so a wake-sock row — the EVIDENCE channel — could vanish with no
        // trace. `FileStore::append_audit` is the single owner: owner-only dir
        // and file, and it warns on every failure as well as returning the
        // error. This caller holds no `&self` (that is why the free function
        // exists at all), so it wraps the same root in a throwaway store
        // instead of carrying a second copy of the writer.
        let store = FileStore::new(state_dir.to_path_buf());
        let path = store.audit_file(workspace);
        // Deliberate fire-and-forget: append_audit eprintln!s the cause on
        // every failure path AND counts it (crate::store::audit_write_failures),
        // so no caller can silently drop a journal write anymore.
        let _ = store.append_audit(workspace, entry);
        // bd-lorot: the audit log had NO rotation while the journal had a full
        // retention discipline. See rotate_audit_if_oversized for why that is
        // backwards: the audit log is the high-volume one.
        rotate_audit_if_oversized(&path);
    }

    /// Presence prompt filter — mirrors `workspace::filter_prompts` over
    /// the sticky presence map: Idle keeps, Busy+urgent breaks through,
    /// everything else becomes a digest-visible `prompt_filtered` Audit
    /// with identical kind/pin/detail text.
    fn apply_presence_filter(&self, effects: Vec<Effect>) -> Vec<Effect> {
        effects
            .into_iter()
            .flat_map(|e| match e {
                // governed-by: bd-fuql — ONE routing decide: presence ×
                // urgency × route × budget composes in doorbell_core.
                // Staged seam: this path resolves route+budget upstream
                // (SelfIndex-gated targets; Brain-owned budgets), so both
                // read true here and the vocabulary swaps with zero
                // behavior drift (parity tests prove it). Threading the
                // real budget signal (Queue) is the follow-up.
                Effect::Prompt { pin, line } => {
                    // bd-3vr9: live herdr status outranks a stale presence
                    // row — but ONLY for captain-pane-unioned pins, whose
                    // panes run no presence hooks (self-report goes stale
                    // forever). Spawned crews keep the presence contract
                    // (bd-v8vi): fresh self-report wins transient races,
                    // absent presence holds.
                    let now = Self::now_unix();
                    let presence_stale = self
                        .presence_ts
                        .get(&pin)
                        .is_none_or(|t| now.saturating_sub(*t) >= PRESENCE_STALE_SECS);
                    let unioned = self.captain_routed_pins.contains(&pin);
                    let presence = if unioned && presence_stale {
                        self.herdr
                            .get(&pin)
                            .map(|s| doorbell_core::wake_core::parse_presence_word(s))
                            .or_else(|| self.presence.get(&pin).copied())
                            .unwrap_or(PresenceVerdict::Unknown)
                    } else {
                        self.presence
                            .get(&pin)
                            .copied()
                            .unwrap_or(PresenceVerdict::Unknown)
                    };
                    let input = doorbell_core::wake_core::RouteInput {
                        pin: pin.clone(),
                        line: line.clone(),
                        urgent: doorbell_core::is_urgent_line(&line),
                        presence,
                        has_route: true,
                        budget_allows: true,
                    };
                    // governed-by: bd-0nem — single fold through the fenced
                    // kernel (was a direct free-fn call; same decision,
                    // journal-shaped effects).
                    let (_, effects) = doorbell_core::wake_core::tick(
                        &doorbell_core::wake_core::WakeModel::default(),
                        &doorbell_core::wake_core::WakeMsg::Route { input },
                    );
                    let decision = effects.into_iter().find_map(|e| match e {
                        doorbell_core::wake_core::WakeEffect::Routed { decision } => Some(decision),
                        _ => None,
                    });
                    // governed-by: bd-zawk — same typed gate as the herdr
                    // path: Busy+urgent breaks through, everything else digests.
                    match decision {
                        Some(decision) => match decision {
                            doorbell_core::wake_core::GateDecision::Inject { pin, line } => {
                                vec![Effect::Prompt { pin, line }]
                            }
                            // bd-v8vi: a held-for-digest wake must actually
                            // digest — the Audit alone left BrightDog urgents
                            // visible nowhere (no Digest, no last_wake). Both
                            // rows journal; absorb_digests delivers the Digest.
                            doorbell_core::wake_core::GateDecision::Digest { pin, reason } => {
                                vec![
                                    Effect::Audit {
                                        kind: "prompt_filtered".into(),
                                        pin: pin.clone(),
                                        detail: serde_json::json!(format!(
                                            "non-injectable presence held wake for digest: {line}"
                                        )),
                                    },
                                    Effect::Digest {
                                        pin,
                                        line,
                                        why: reason,
                                    },
                                ]
                            }
                            // Unreachable at this seam (budget/route read true
                            // above): degrade LOUD to digest, never silent drop
                            // and never an unbudgeted prompt.
                            doorbell_core::wake_core::GateDecision::Queue { wake } => vec![
                                Effect::Audit {
                                    kind: "prompt_filtered".into(),
                                    pin: pin.clone(),
                                    detail: serde_json::json!(format!(
                                        "budget-held wake queued for digest: {wake}"
                                    )),
                                },
                                Effect::Digest {
                                    pin,
                                    line: wake,
                                    why: "budget spent: queued wake surfaces in digest".to_string(),
                                },
                            ],
                            doorbell_core::wake_core::GateDecision::Drop { reason } => {
                                vec![Effect::Audit {
                                    kind: "prompt_filtered".into(),
                                    pin,
                                    detail: serde_json::json!(format!(
                                        "wake dropped at routing: {reason}"
                                    )),
                                }]
                            }
                        },
                        None => vec![],
                    }
                }
                other => vec![other],
            })
            .collect()
    }

    fn normalize(&self, actions: Vec<doorbell_core::DoorbellAction>) -> Vec<Effect> {
        actions
            .into_iter()
            .map(|a| match a {
                doorbell_core::DoorbellAction::Prompt { pin, line } => Effect::Prompt { pin, line },
                // Signal only: the brain fetches every known pin with
                // per-pin cursors (mirrors the embedded mapping).
                doorbell_core::DoorbellAction::FetchInbox => Effect::FetchInbox,
                doorbell_core::DoorbellAction::CheckAckSla => Effect::CheckAckSla,
                doorbell_core::DoorbellAction::RestartWatch => Effect::RestartWatch,
                doorbell_core::DoorbellAction::FactMiss { why } => Effect::Audit {
                    kind: "fact_miss".into(),
                    pin: String::new(),
                    detail: serde_json::json!({ "why": why }),
                },
                doorbell_core::DoorbellAction::Audit { kind, pin, detail } => Effect::Audit {
                    kind,
                    pin,
                    detail: serde_json::json!(detail),
                },
                doorbell_core::DoorbellAction::Digest { pin, line, why } => {
                    Effect::Digest { pin, line, why }
                }
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
            pre.push(Effect::Audit {
                kind,
                pin: String::new(),
                detail,
            });
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
            BrainMsg::Sweep { .. } => vec![Effect::FetchInbox],
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
                // the embedded workspace arm — precedence + line shape come
                // from the shared core helpers (bd-z5ou), only the Idle
                // read is host-side. Without this every daemon-side fact —
                // including `inject` — degraded to fact_miss even with a
                // bound captain.
                if let Some(pin) = doorbell_core::fact_captain_pin(&self.roles)
                    && self.presence.get(&pin) == Some(&PresenceVerdict::Idle)
                {
                    self.apply_presence_filter(vec![Effect::Prompt {
                        pin,
                        line: doorbell_core::fact_prompt_line(summary),
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
                self.presence
                    .insert(pin.clone(), doorbell_core::parse_presence_word(state));
                self.presence_ts.insert(pin.clone(), Self::now_unix());
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
            // bd-w98c residual: same decision as the embedded host — the hit
            // list audits (LoudMiss-free data audit) + asks for the inbox so
            // overdue rows can re-enter. Both hosts answer FetchInbox so the
            // parity assertion on AckSlaTick stays an honest mirror.
            BrainMsg::SlaFired { posts } => vec![
                Effect::Audit {
                    kind: "ack-sla-hit".into(),
                    pin: String::new(),
                    detail: serde_json::json!({ "overdue": posts }),
                },
                Effect::FetchInbox,
            ],
            BrainMsg::Degraded { .. }
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
            let error = String::from_utf8_lossy(&out.stderr).to_string();
            self.audit_file(
                inbox_fail_kind(&error),
                serde_json::json!({
                    "pin": pin,
                    "error": error,
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
            let error = String::from_utf8_lossy(&out.stderr).to_string();
            // bd-r39p: name the skew here too. This arm opens the SAME shared
            // store as the inbox arm, so it refuses for the same reason, but it
            // wrote `ack_sla_error` and the fault was invisible under a probe
            // keyed on the inbox kind.
            self.audit_file(
                if is_store_skew_refusal(&error) {
                    "ack_sla_store_skew"
                } else {
                    "ack_sla_error"
                },
                serde_json::json!({
                    "error": error,
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

    /// Returns `true` when this death took the restart slot, `false` when it
    /// coalesced into one already pending — the brain journals a restart row
    /// only for `true` (bd-watch-restart-metric-8dvf).
    fn restart_wake(&mut self) -> bool {
        // SINGLE-FLIGHT, before anything else. A burst of `Died` collapses to
        // one scheduled restart. Without this the backoff spaces nothing: every
        // death brings its own timer, and they all fire together after the
        // sleep — which is exactly how 1,613 connects landed in one second.
        use std::sync::atomic::Ordering;
        if self.restart_pending.swap(true, Ordering::SeqCst) {
            return false;
        }
        // The 16s cap is deliberate (GS-007 never-deaf: probing forever beats
        // going wake-blind), so the loop itself is not the bug. What was broken
        // is that the gate never opened again — see the `retry` field.
        let backoff = self.retry.lock().map(|mut r| r.record_fail()).unwrap_or(16);
        // bd-68q2: sample a digest naming the live rate, the fd census, and
        // the backoff — one row per power-of-two fails, decided by fails, not
        // by wall clock, so a storm reads as a rate in the audit log.
        let fails = self.retry.lock().map(|r| r.fails()).unwrap_or(0);
        if should_audit_count(fails as usize) {
            Self::append_audit_row(
                &self.cfg.flywheel_state,
                &self.cfg.workspace,
                AUDIT_WAKE_FAILING_STILL,
                failing_still_row(fails as u64, crate::wake_sock::open_fd_count(), backoff),
            );
        }
        let tx = self.tx.clone();
        let project = self.cfg.project.clone();
        let fw_state = self.cfg.flywheel_state.clone();
        let workspace = self.cfg.workspace.clone();
        // Host-wide, so a storm across consumers reads as one rate. Hoisted for
        // the same 'static reason as the clones above.
        let died_relayed = self.died_relayed.clone();
        // One live consumer per host, never a litter. The slot aborts the
        // previous task before the replacement subscribes.
        let slot = self.wake_task.clone();
        // Hoisted OUT of the spawned future: the future is 'static, so reading
        // a field off `&mut self` inside it would let the borrow escape.
        let retry = self.retry.clone();
        // Hoisted out of the 'static future so no borrow of `self` escapes.
        let pending = self.restart_pending.clone();
        let absent = self.absent_rows.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            // The slot frees the moment we are about to ACT, so the next death
            // schedules its own restart instead of being dropped. Clearing it
            // after the sleep is what makes this a rate limit rather than a
            // one-shot: during the sleep deaths coalesce, after it they queue.
            pending.store(false, std::sync::atomic::Ordering::SeqCst);
            let Some(tx) = tx else { return };
            if let Ok(mut g) = slot.lock()
                && let Some((h, retire)) = g.take()
            {
                // bd-v7uu: mark the predecessor RETIRED before aborting it. An
                // abort closes the channel with no terminal outcome, which the
                // consumer's monitor cannot otherwise distinguish from a crash — so
                // every intentional restart logged `wake_consumer_dropped` and
                // forwarded a synthetic `Died` that advanced the retry backoff. The
                // recovery was counting itself as a failure.
                retire.store(true, std::sync::atomic::Ordering::SeqCst);
                h.abort();
            }
            let opts = crate::wake_sock::WakeOpts::new(
                crate::wake_sock::wake_sock_path(),
                crate::wake_sock::parse_kinds(WATCH_KINDS),
                Some(project),
            );
            let audit_state = fw_state.clone();
            let audit_workspace = workspace.clone();
            // `audit_state`/`audit_workspace` are moved into the on_audit
            // closure below, so the outcome arm keeps its own handles for the
            // drop audit. Without an audit there, a full channel was invisible.
            let full_state = fw_state.clone();
            let full_workspace = workspace.clone();
            let beat_state = fw_state.clone();
            let beat_workspace = workspace.clone();
            // One death report per consumer: the flood was one consumer hitting
            // one full channel, reported as a death per line.
            let reported_death = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            // `retry` is hoisted before the spawn; re-reading `self` here would
            // let the borrow escape the 'static future.
            let absent_rows = absent.clone();
            let absent_for_audit = absent.clone();
            let retire = opts.retire_flag();
            let handle = crate::wake_sock::spawn_wake_task(
                opts,
                move |kind, detail| {
                    // bd-lorot: ONE row per outage for a repeated absence.
                    // Measured live before this: 1,613 identical
                    // `wake_sock_absent` rows in ONE second = 169 KB/s, which
                    // tripped the 5 MiB rotation into rewriting the file
                    // continuously. The cause is worth recording once; the same
                    // cause 1,613 times is a spin loop, and one a peer service
                    // can induce is a denial-of-service surface.
                    if kind == crate::wake_sock::AUDIT_SOCK_ABSENT
                        && absent_for_audit.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
                    {
                        return;
                    }
                    FileHost::append_audit_row(&audit_state, &audit_workspace, kind, detail);
                },
                move |out| {
                    use crate::wake_sock::WakeOutcome;
                    // GS062-ADV-5 (ported onto wake-sock): try_send Watch (never
                    // park); stamp .event only after Ok; Died via blocking_send
                    // so Full cannot drop restart_wake.
                    match out {
                        WakeOutcome::Watch(l) => match tx.try_send(BrainMsg::Watch(l)) {
                            Ok(()) => {
                                crate::watchbeat::touch_event(&beat_state, &beat_workspace);
                                // A delivered event is the only trustworthy
                                // evidence the socket is live again, so it is
                                // what reopens the gate. Nothing else may:
                                // a heartbeat proves the ticker ran, not that
                                // the transport delivers.
                                if let Ok(mut r) = retry.lock() {
                                    r.reset();
                                }
                                // The outage is over, so the NEXT absence must be
                                // audited again rather than swallowed by a counter
                                // the last outage left high.
                                absent_rows.fetch_and(0, std::sync::atomic::Ordering::SeqCst);
                            }
                            Err(_) => {
                                // The line could not be enqueued. Report ONE
                                // death for this consumer so a restart still
                                // happens and the socket's cursor replays what
                                // was missed, then count the rest instead of
                                // manufacturing a death per line.
                                match full_channel_action(&reported_death) {
                                    FullChannelAction::ReportDeath => {
                                        FileHost::append_audit_row(
                                            &full_state,
                                            &full_workspace,
                                            AUDIT_BRAIN_FULL,
                                            serde_json::json!({
                                                "reason": "watch line could not be enqueued (brain channel full); reporting one death so the consumer restarts and replays from the cursor",
                                            }),
                                        );
                                        crate::msg::deliver_died_blocking(&tx);
                                    }
                                    FullChannelAction::CountDropped => {
                                        let prior = dropped
                                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        if should_audit_count(prior) {
                                            FileHost::append_audit_row(
                                                &full_state,
                                                &full_workspace,
                                                AUDIT_BRAIN_FULL,
                                                serde_json::json!({
                                                    "reason": "further watch lines could not be enqueued",
                                                    "dropped": prior + 1,
                                                }),
                                            );
                                        }
                                    }
                                }
                            }
                        },
                        WakeOutcome::Heartbeat => {
                            crate::watchbeat::touch_heartbeat(&beat_state, &beat_workspace);
                        }
                        WakeOutcome::Died => {
                            // The relay is where a death storm was previously
                            // invisible, so it is where the rate gets named.
                            // Delivery stays unconditional: the restart depends
                            // on the death arriving, so only the audit is
                            // sampled, never the relay.
                            let prior =
                                died_relayed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if let DiedRelayAction::RelayAndAudit(relayed) =
                                died_relay_action(prior)
                            {
                                FileHost::append_audit_row(
                                    &full_state,
                                    &full_workspace,
                                    AUDIT_DIED_RELAYED,
                                    serde_json::json!({
                                        "reason": "consumer deaths relayed to the brain",
                                        "relayed": relayed,
                                    }),
                                );
                            }
                            crate::msg::deliver_died_blocking(&tx);
                        }
                    }
                },
            );
            if let Ok(mut g) = slot.lock() {
                *g = Some((handle, retire));
            }
        });
        true
    }

    fn inject_allowed(&self, _pin: &str) -> bool {
        match &self.generation {
            // Daemon-keyed generation: stale binary refuses wakes.
            Some(record) => !gen_stale(record, &self.daemon_exe),
            None => true,
        }
    }

    /// bd-bk9t: routability IS the routing table the prompt sink reads — a
    /// subscribed pin with no registry row has no entry here, so a digest
    /// batch for it has nowhere to be shipped (see `Host::has_route`).
    fn has_route(&self, pin: &str) -> bool {
        self.targets
            .lock()
            .map(|t| t.contains_key(pin))
            .unwrap_or(false)
    }

    /// bd-8ulu: the subscription row's sink wins over the workspace default,
    /// so a pane-less operator pin subscribed to the webhook sink lands there.
    fn sink_for(&self, pin: &str) -> Option<String> {
        FileHost::sink_for(self, pin)
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
                match tx.try_send(BrainMsg::HerdrStatus {
                    pin: pin.clone(),
                    status: status.clone(),
                }) {
                    Ok(()) => {}
                    Err(e) => {
                        // Self-healing: self.herdr advances only on receipt
                        // (the BrainMsg::HerdrStatus arm), so a dropped update
                        // is recomputed as changed next sweep — delay, not
                        // loss. Count every drop and audit sampled, the same
                        // repeat-is-a-count shape as wake_brain_full.
                        let closed = matches!(e, tokio::sync::mpsc::error::TrySendError::Closed(_));
                        let prior = self
                            .herdr_dropped
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if should_audit_count(prior) {
                            Self::append_audit_row(
                                &self.cfg.flywheel_state,
                                &self.cfg.workspace,
                                AUDIT_HERDR_FULL,
                                serde_json::json!({
                                    "reason": "herdr status change could not be enqueued (brain channel full or closed); next sweep retries",
                                    "dropped": prior + 1,
                                    "closed": closed,
                                }),
                            );
                        }
                    }
                }
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

    /// bd-jcys: `append_audit_row` is the wake-sock task's only audit door,
    /// and it used to be a SECOND implementation that swallowed its own
    /// failures. It now delegates to `FileStore::append_audit`, so the row
    /// lands 0600 inside a 0700 directory like every other audit row — the
    /// shape a perms audit and a backup both rely on. Fork the writer again
    /// and this goes red.
    #[cfg(unix)]
    #[test]
    fn wake_sock_audit_row_rides_the_one_audit_writer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dbrt-auditrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        FileHost::append_audit_row(&dir, "w", "probe", serde_json::json!({"n": 1}));
        let file = dir.join("doorbell").join("w.audit.jsonl");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600, "the row lands owner-only");
        assert_eq!(mode(&dir.join("doorbell")), 0o700, "in an owner-only dir");
        let body = std::fs::read_to_string(&file).expect("audit row");
        assert!(
            body.contains("\"kind\":\"probe\"") && body.contains("\"n\":1"),
            "and keeps the row shape the callers pass: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-239z: `run_cli` piped both streams but read them only AFTER the
    /// child exited. A pipe is a finite buffer (65536 bytes measured on this
    /// host), so a child writing past it blocked inside `write`, never exited,
    /// and was killed at the deadline with every byte it had produced thrown
    /// away. The daemon read that as "toron spawn/timeout failed" — the
    /// opposite of the truth, and a cold 50-row `mail inbox` page (88925 bytes)
    /// is enough to trigger it, so the pin's first fetch was the last one that
    /// could ever work and the retry was equally cold. Self-reinforcing.
    #[cfg(unix)]
    #[test]
    fn run_cli_collects_output_larger_than_the_pipe_capacity() {
        let dir = std::env::temp_dir().join(format!("dbrt-pipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let script = dir.join("loud.sh");
        // 4 x 64 KiB on stdout (three pipe-buffer loads past the first) and a
        // tail on stderr, so the test proves BOTH pipes drained, not just one.
        std::fs::write(
            &script,
            "#!/bin/sh\ndd if=/dev/zero bs=65536 count=4 2>/dev/null\necho marker-on-stderr >&2\n",
        )
        .expect("script");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let out = run_cli(script.to_str().expect("utf-8 path"), &[], 10)
            .expect("a child that exits on its own is collected, not killed and discarded");
        assert!(
            out.status.success(),
            "the script is not the thing under test"
        );
        assert_eq!(
            out.stdout.len(),
            4 * 65536,
            "every byte past the pipe capacity is collected"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("marker-on-stderr"),
            "and stderr is drained too, or it fills its own pipe the same way"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-239z, the other half: draining must not turn the deadline into a lie.
    /// `sleep` inherits the pipe write end, so a reader thread joined AFTER the
    /// kill would sit there for the sleep's full duration — the timeout would
    /// silently become 30s and a hung sweep would pile up behind it. The
    /// deadline has to end the wait whether or not the output was ever wanted.
    #[cfg(unix)]
    #[test]
    fn run_cli_deadline_still_bounds_a_child_that_never_exits() {
        let dir = std::env::temp_dir().join(format!("dbrt-hang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let script = dir.join("hang.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("script");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let started = std::time::Instant::now();
        let out = run_cli(script.to_str().expect("utf-8 path"), &[], 1);
        assert!(
            out.is_none(),
            "past the deadline the run is refused, not awaited"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "and the wait ends at the deadline, not when the orphaned writer does: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-r39p: the ack-SLA arm opens the SAME shared store as the inbox arm, so
    /// it refuses for the same reason — but it wrote `ack_sla_error` and a
    /// `/health` probe keyed on the inbox kind never saw it. Live 2026-09-28:
    /// the store refused every minute and the health body read `degraded: []`.
    /// The row kind is what an operator reads, so it has to name the fault.
    #[cfg(unix)]
    #[test]
    fn ack_sla_refusal_names_the_store_skew_it_shared_with_the_inbox_arm() {
        let dir = std::env::temp_dir().join(format!("dbrt-acksla-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let script = dir.join("skewed-toron.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'toron refuses this store: the store is at migration 14 \
             and this binary resolves at most 13 [toron-error=store_skew]' >&2\nexit 1\n",
        )
        .expect("script");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "ws-a");
        cfg.toron_bin = script.to_string_lossy().into_owned();
        let mut host = FileHost::new(cfg);
        assert!(
            host.scan_ack_sla().is_empty(),
            "a refusing scan yields no overdue ids — it never invents them"
        );
        let body =
            std::fs::read_to_string(dir.join("doorbell/ws-a.audit.jsonl")).expect("audit row");
        assert!(
            body.contains("\"kind\":\"ack_sla_store_skew\""),
            "the refusal is named, not filed under a kind that says nothing: {body}"
        );
        assert!(
            body.contains(TORON_STORE_SKEW_MARKER),
            "and the row carries the marker, so a kind-independent probe finds it: {body}"
        );
        // A different refusal keeps its own kind: this arm is not a skew oracle.
        std::fs::write(&script, "#!/bin/sh\necho 'no such flag' >&2\nexit 2\n").expect("script");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let mut host = FileHost::new(FileHostConfig::new(dir.clone(), "proj", "ws-b"));
        host.cfg.toron_bin = script.to_string_lossy().into_owned();
        assert!(host.scan_ack_sla().is_empty());
        let body =
            std::fs::read_to_string(dir.join("doorbell/ws-b.audit.jsonl")).expect("audit row");
        assert!(
            body.contains("\"kind\":\"ack_sla_error\""),
            "an unrelated failure stays an ack_sla_error: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-3vr9: the captain-pane union. A swept pin with no registry
    /// route binds the pane whose herdr `toron_pin` token names it, so
    /// has_route holds and the wake prompts. Pins already routed, pins
    /// nobody names, and token-less crew panes are untouched — an
    /// unroutable pin keeps exactly one fate: digest-bound.
    #[test]
    fn captain_pane_joins_target_set_by_token() {
        let roster = vec![
            ("w6:p1".into(), "idle".into(), Some("BrightBadger".into())),
            ("w1W:p1".into(), "working".into(), None),
        ];
        let want = vec!["BrightBadger".to_string()];
        let routed: std::collections::HashSet<String> =
            ["DustyReef".to_string()].into_iter().collect();
        let ops = captain_pane_targets(&roster, &want, &routed);
        assert_eq!(ops, vec![("BrightBadger".to_string(), "w6:p1".to_string())]);
    }

    #[test]
    fn captain_union_skips_routed_and_unnamed() {
        let roster = vec![
            ("w6:p1".into(), "idle".into(), Some("BrightBadger".into())),
            ("w6:p2".into(), "idle".into(), None),
            ("w6:p3".into(), "idle".into(), Some("Stranger".into())),
        ];
        let want = vec!["BrightBadger".to_string(), "Gone".to_string()];
        // Already registry-routed: token must not double-bind.
        let routed: std::collections::HashSet<String> =
            ["BrightBadger".to_string()].into_iter().collect();
        assert!(captain_pane_targets(&roster, &want, &routed).is_empty());
        // Unwanted pins and the token-less pane never route: Gone stays
        // digest-bound, never queued into a table that cannot drain it.
        let empty: std::collections::HashSet<String> = Default::default();
        assert!(captain_pane_targets(&roster, &["Gone".to_string()], &empty).is_empty());
    }

    /// bd-523f: the LAST hop dies on the TARGET SPELLING, not on the route.
    /// The routing table's value is handed verbatim to `herdr agent prompt`,
    /// and herdr resolves an agent by its herdr LABEL or by PANE ID. A pane
    /// nobody started with `herdr agent start` — every operator pane — has no
    /// label: on this host `herdr agent get w6:p1` resolves the captain's pane
    /// while `herdr agent get AmberQuill` answers `agent_not_found`, and the
    /// last recorded delivery attempt is
    /// `degraded: delivery failed: herdr prompt failed: agent target
    /// BlackOrchid not found`. The captain union already stores pane ids
    /// (bd-3vr9); the registry half stored the label, so one fleet, one
    /// routing table, two unresolvable spellings.
    #[test]
    fn prompt_hop_routes_a_registry_row_by_pane_id_not_by_label() {
        let (mut host, dir) = host_with_registry("prompt-hop-target", entries());
        host.refresh_index();
        let targets = host.targets.lock().expect("targets lock");
        assert_eq!(
            targets.get("PinA").map(String::as_str),
            Some("pane-a"),
            "the sink must be handed the pane id herdr resolves, not the label"
        );
        assert_eq!(targets.get("PinB").map(String::as_str), Some("pane-b"));
        drop(targets);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The unhappy path of the same rule: a registry row with NO pane has
    /// nothing to prompt. Falling back to the label keeps the one shape herdr
    /// can still resolve (a crew row written before panes were recorded)
    /// instead of handing the sink an empty target that reads as a route.
    #[test]
    fn prompt_hop_keeps_the_label_when_the_row_has_no_pane() {
        let (mut host, dir) = host_with_registry(
            "prompt-hop-nopane",
            serde_json::json!([{"pane_id": "", "name": "herdr-c", "mail_identity": "PinC"}]),
        );
        host.refresh_index();
        let targets = host.targets.lock().expect("targets lock");
        assert_eq!(
            targets.get("PinC").map(String::as_str),
            Some("herdr-c"),
            "a pane-less row keeps the label it has"
        );
        drop(targets);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roster_parse_reads_pin_token_and_skips_bare_panes() {
        let v = serde_json::json!({
            "result": {"agents": [
                {"pane_id": "w6:p1", "agent_status": "idle",
                 "tokens": {"toron_pin": "BrightBadger"}},
                {"pane_id": "w1W:p1", "agent_status": "working"},
                {"agent_status": "idle"},
            ]}
        });
        assert_eq!(
            parse_agent_roster(&v),
            vec![
                (
                    "w6:p1".to_string(),
                    "idle".to_string(),
                    Some("BrightBadger".to_string())
                ),
                ("w1W:p1".to_string(), "working".to_string(), None),
            ]
        );
    }

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

    /// The registry has no garbage collection, so an ambiguity can persist for
    /// days. Auditing it once per sweep wrote 1,644 rows in 45 minutes, every
    /// one naming the same file — a row every 1.7s, ~52k a day — and trained
    /// the reader to skip it. Deduped to one row per outage, which is the rule
    /// the socket's own absent-audit already follows.
    #[test]
    fn a_persistent_registry_ambiguity_is_audited_once_not_per_sweep() {
        let dir = tmp_state("registry-drip");
        for hashed in ["proj-aaaaaaaa", "proj-bbbbbbbb"] {
            std::fs::write(
                dir.join("agents").join(format!("{hashed}.json")),
                serde_json::to_string(&entries()).unwrap(),
            )
            .unwrap();
        }
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = "true".into();
        cfg.herdr_bin = "true".into();
        let mut host = FileHost::new(cfg);

        let mut audits = 0;
        for _ in 0..36 {
            let _ = host.registry_rows();
            if host.pending_audit.take().is_some() {
                audits += 1;
            }
        }
        assert_eq!(
            audits, 1,
            "36 sweeps of one persistent ambiguity wrote {audits} audit rows; live it ran \
             once per 1.7s for a day, 52k rows all naming a single file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn entries() -> serde_json::Value {
        serde_json::json!([
            {"pane_id": "pane-a", "name": "herdr-a", "mail_identity": "PinA"},
            {"pane_id": "pane-b", "name": "herdr-b", "mail_identity": "PinB"}
        ])
    }

    /// bd-adms: the registry is not garbage-collected. Live, three pins sat
    /// on `w11:*` panes herdr had long GC'd, so every tick fetched dead
    /// inboxes (and their failed fetches wrote the `inbox_error` stream that
    /// hid a real outage). A pin whose pane is gone must leave the sweep set,
    /// ONCE and visibly — and a pin whose pane is present must never be touched.
    ///
    /// bd-3uqz: this used to advance a CLOCK past STALE_PANE_GRACE_SECS. The
    /// evidence is now a count of CONSECUTIVE sweeps, so the test drives sweeps
    /// instead. The intent is unchanged and still worth asserting; only the driver
    /// moved. A clock is a proxy for "we have seen this repeatedly" and it is wrong
    /// in both directions — one herdr outage outlasting the window prunes a live
    /// fleet, and a pane that died and returned inside the window is never pruned.
    #[test]
    fn stale_registry_pins_leave_the_sweep_after_three_confirmations() {
        let dir = tmp_state("reap");
        std::fs::write(
            dir.join("agents").join("proj.json"),
            serde_json::to_string(&serde_json::json!([
                {"pane_id": "w9:p1", "name": "herdr-a", "mail_identity": "LivePin"},
                {"pane_id": "w11:p1", "name": "herdr-dead", "mail_identity": "DeadPin"},
            ]))
            .unwrap(),
        )
        .unwrap();
        // `herdr pane list` knows only w9:p1; `agent list` keeps w9:p1 idle so
        // the live pin stays injectable.
        let script = dir.join("herdr");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = \"pane\" ]; then echo '{\"result\":{\"panes\":[{\"pane_id\":\"w9:p1\"}]}}'; \
             else echo '{\"result\":{\"agents\":[{\"pane_id\":\"w9:p1\",\"agent_status\":\"idle\"}]}}'; fi\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = "true".into();
        cfg.herdr_bin = script.to_string_lossy().into_owned();
        cfg.registry_file = Some(dir.join("agents").join("proj.json"));
        let mut host = FileHost::new(cfg);
        host.refresh_index();
        assert_eq!(
            host.pins(),
            vec!["LivePin".to_string(), "DeadPin".to_string()],
            "the first sighting of a vanished pane is not evidence"
        );
        // bd-3uqz: drive CONSECUTIVE SWEEPS, not a clock. The index is TTL-gated,
        // so each pass has to age out before the next one is a real observation.
        //
        // The refresh_index() above was ALREADY the first sighting, so
        // REQUIRED_CONFIRMATIONS - 1 more are needed. Written as a decrement so the
        // arithmetic cannot drift: my first conversion looped the full count and
        // pruned one sweep early, because the initial refresh was silently the
        // first observation.
        let mut needed = crate::registry_prune::REQUIRED_CONFIRMATIONS - 1;
        while needed > 0 {
            let birth = FileHost::now_unix();
            host.index_refreshed_at = birth - INDEX_TTL_SECS - 1;
            host.refresh_index();
            needed -= 1;
            if needed > 0 {
                assert!(
                    host.pins().contains(&"DeadPin".to_string()),
                    "{needed} confirmation(s) still outstanding: an unconfirmed pane must \
                     stay in the sweep"
                );
            }
        }
        assert_eq!(
            host.pins(),
            vec!["LivePin".to_string()],
            "three consecutive confirmations of a vanished pane is positive evidence of death"
        );
        assert!(
            host.reducer.model.unroutable.contains("DeadPin"),
            "a vanished pane has no route, so its wake digests"
        );
        let audit = std::fs::read_to_string(dir.join("doorbell").join("w.audit.jsonl")).unwrap();
        assert_eq!(
            audit.matches("registry_pin_stale").count(),
            1,
            "one audit row, not one per tick: {audit}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-ba8t: `herdr agent list` returning no agents is the live condition
    /// that made the ring's last hop impossible and invisible at once. The
    /// picture must say `panes_seen: 0` rather than "all busy".
    #[test]
    fn injection_picture_sees_zero_panes_when_herdr_has_no_agents() {
        let dir = tmp_state("inj-none");
        std::fs::write(
            dir.join("agents").join("proj.json"),
            serde_json::to_string(&entries()).unwrap(),
        )
        .unwrap();
        let script = dir.join("herdr-empty");
        std::fs::write(&script, "#!/bin/sh\necho '{\"result\":{\"agents\":[]}}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        let pic = injection_picture(
            &dir,
            "proj",
            Some(&dir.join("agents").join("proj.json")),
            &script.to_string_lossy(),
        );
        assert_eq!(pic.pins, 2, "two registry pins");
        assert_eq!(pic.panes_seen, 0, "herdr saw no agents");
        assert_eq!(pic.injectable, 0);
        assert_eq!(pic.panes_missing, 2, "both panes unaccounted for");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The healthy shape the signal must NOT fire on: herdr knows the panes
    /// (some busy, some idle), so the ring can land.
    #[test]
    fn injection_picture_counts_busy_and_idle_panes() {
        let dir = tmp_state("inj-mixed");
        std::fs::write(
            dir.join("agents").join("proj.json"),
            serde_json::to_string(&entries()).unwrap(),
        )
        .unwrap();
        let script = dir.join("herdr-mixed");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"result\":{\"agents\":[{\"pane_id\":\"pane-a\",\"agent_status\":\"working\"},{\"pane_id\":\"pane-b\",\"agent_status\":\"idle\"}]}}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        let pic = injection_picture(
            &dir,
            "proj",
            Some(&dir.join("agents").join("proj.json")),
            &script.to_string_lossy(),
        );
        assert_eq!(
            (pic.pins, pic.panes_seen, pic.injectable, pic.panes_missing),
            (2, 2, 1, 0)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-2i96 G4: `pins` tracks the sweep set, not the raw registry. A
    /// pin whose pane herdr no longer lists is reaped from the sweep, so
    /// /health must stop counting it in pins. The fault stays visible in
    /// `panes_missing`, never in `pins`.
    #[test]
    fn injection_picture_excludes_stale_panes_from_pin_count() {
        let dir = tmp_state("inj-stale");
        std::fs::write(
            dir.join("agents").join("proj.json"),
            serde_json::to_string(&entries()).unwrap(),
        )
        .unwrap();
        // pane-b is dead: absent from both the pane list and the agents.
        let script = dir.join("herdr-stale");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = \"pane\" ]; then echo '{\"result\":{\"panes\":[{\"pane_id\":\"pane-a\"}]}}'; else echo '{\"result\":{\"agents\":[{\"pane_id\":\"pane-a\",\"agent_status\":\"idle\"}]}}'; fi\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        let pic = injection_picture(
            &dir,
            "proj",
            Some(&dir.join("agents").join("proj.json")),
            &script.to_string_lossy(),
        );
        assert_eq!(
            (pic.pins, pic.panes_seen, pic.injectable, pic.panes_missing),
            (1, 1, 1, 1),
            "stale pane-b leaves pins, fault stays in panes_missing"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// bd-cak3: the registry only knows SPAWNED crews. A subscribed identity
    /// that was never spawned in this project (the operator's own pin is the
    /// live case) had mail delivered, a subscription row, and no sweep, no
    /// `SelfIndex` entry, and no way to be addressed — so its wake never
    /// materialised at all. The index must union subscribed pins in.
    #[test]
    fn subscribed_pin_without_a_registry_row_enters_the_index() {
        use crate::subscribe::{
            Actor, Budget, Filter, Identity, Scope, SinkKind, Subscription, SubscriptionStore,
        };
        let (mut host, dir) = host_with_registry("cak3", entries());
        let mut store = SubscriptionStore::open(&dir);
        store
            .subscribe(
                Actor::local_process(),
                Subscription {
                    id: "cli-proj-w-OperatorPin".into(),
                    identity: Identity::Pin("OperatorPin".into()),
                    scope: Scope::One("w".into()),
                    filter: Filter::default(),
                    sink: SinkKind::Herdr,
                    budget: Budget::default_530(),
                    project: Some("proj".into()),
                    endpoint: None,
                },
                "w",
            )
            .expect("local herdr subscribe");
        host.refresh_index();
        assert!(
            host.pins().contains(&"OperatorPin".to_string()),
            "the sweep set must union subscribed pins: {:?}",
            host.pins()
        );
        assert_eq!(
            host.reducer.model.index.resolve("OperatorPin").as_deref(),
            Some("OperatorPin"),
            "a subscribed pin must be addressable (classifier self-index)"
        );
        // ...but never a FABRICATED route: a pane-less identity must not be
        // handed a prompt target the sink cannot deliver to.
        assert!(
            !host.panes.values().any(|p| p == "OperatorPin"),
            "no pane is invented for a subscribed pin"
        );
        assert!(
            !host
                .targets
                .lock()
                .expect("targets lock")
                .contains_key("OperatorPin"),
            "no herdr target is invented for a subscribed pin"
        );
        // bd-bk9t: and because there is no route, the index marks it
        // unrouteable so its wakes are digest-bound instead of parked in a
        // queue no status transition can ever drain.
        assert!(
            host.reducer.model.unroutable.contains("OperatorPin"),
            "a pane-less subscribed pin must be marked unroutable: {:?}",
            host.reducer.model.unroutable
        );
        // A spawned pin with a pane keeps its route.
        assert!(
            !host.reducer.model.unroutable.is_empty()
                && !host.reducer.model.unroutable.contains("PinA"),
            "a registry pin with a pane stays routable: {:?}",
            host.reducer.model.unroutable
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-bk9t: the wake for a pane-less subscribed pin must not enter the
    /// per-pin queue. The queue only drains on `working -> injectable`, which
    /// a pin with no pane can never produce — so a queued wake is a RAM-only
    /// entry that dies with the daemon. It belongs in the digest batch.
    #[test]
    fn pane_less_pin_wake_is_digest_bound_not_queued() {
        use crate::subscribe::{
            Actor, Budget, Filter, Identity, Scope, SinkKind, Subscription, SubscriptionStore,
        };
        let (mut host, dir) = host_with_registry("bk9t", entries());
        let mut store = SubscriptionStore::open(&dir);
        store
            .subscribe(
                Actor::local_process(),
                Subscription {
                    id: "cli-proj-w-OperatorPin".into(),
                    identity: Identity::Pin("OperatorPin".into()),
                    scope: Scope::One("w".into()),
                    filter: Filter::default(),
                    sink: SinkKind::Herdr,
                    budget: Budget::default_530(),
                    project: Some("proj".into()),
                    endpoint: None,
                },
                "w",
            )
            .expect("local herdr subscribe");
        host.refresh_index();
        let fx = host.tick(&BrainMsg::Watch(watch_line(
            "OperatorPin",
            "@OperatorPin ping",
        )));
        assert!(
            fx.iter().any(
                |e| matches!(e, Effect::Digest { pin, why, .. } if pin == "OperatorPin" && why == "no_route")
            ),
            "a pane-less pin's wake is digest-bound with a named reason: {fx:?}"
        );
        assert!(
            !fx.iter()
                .any(|e| matches!(e, Effect::Audit { kind, .. } if kind == "queued")),
            "no queued audit for a pin that can never drain: {fx:?}"
        );
        assert!(
            host.reducer.model.queues.is_empty(),
            "the RAM queue stays empty for digest-only pins: {:?}",
            host.reducer.model.queues
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Scope is per-workspace: another workspace's subscription is not this
    /// host's business and must not leak into its sweep set.
    #[test]
    fn subscribed_pin_of_another_workspace_stays_out() {
        use crate::subscribe::{
            Actor, Budget, Filter, Identity, Scope, SinkKind, Subscription, SubscriptionStore,
        };
        let (mut host, dir) = host_with_registry("cak3-scope", entries());
        let mut store = SubscriptionStore::open(&dir);
        store
            .subscribe(
                Actor::local_process(),
                Subscription {
                    id: "cli-proj-other-Elsewhere".into(),
                    identity: Identity::Pin("Elsewhere".into()),
                    scope: Scope::One("other-ws".into()),
                    filter: Filter::default(),
                    sink: SinkKind::Herdr,
                    budget: Budget::default_530(),
                    project: Some("proj".into()),
                    endpoint: None,
                },
                "other-ws",
            )
            .expect("local herdr subscribe");
        host.refresh_index();
        assert!(
            !host.pins().contains(&"Elsewhere".to_string()),
            "another workspace's subscription is not this host's business: {:?}",
            host.pins()
        );
        let _ = std::fs::remove_dir_all(&dir);
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
    fn unknown_presence_urgent_actually_digests() {
        // bd-v8vi (BrightDog): idle-in-herdr but presence-Unknown — the
        // filter held the urgent "for digest" and digested nothing
        // (audit only). The Digest intent must be emitted, not just audited.
        let (mut host, _dir) = host_with_registry("t-unk", entries());
        host.refresh_index();
        host.tick(&BrainMsg::HerdrStatus {
            pin: "PinA".into(),
            status: "idle".into(),
        });
        // No Presence tick: verdict stays Unknown.
        let rows = vec![serde_json::json!({"importance": "urgent", "body": "x"})];
        let fx = host.tick(&BrainMsg::InboxFetched {
            pin: "PinA".into(),
            rows,
        });
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::Audit { kind, .. } if kind == "prompt_filtered")),
            "held wake stays audited: {fx:?}"
        );
        assert!(
            fx.iter().any(
                |e| matches!(e, Effect::Digest { pin, why, .. } if pin == "PinA" && !why.is_empty())
            ),
            "held wake must actually digest: {fx:?}"
        );
        let _ = std::fs::remove_dir_all(_dir);
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

    /// bd-3vr9: live herdr status outranks a stale presence row.
    /// Presence self-reports only on agent publish; herdr observes every
    /// sweep. Stale Busy presence + live idle herdr must prompt; live
    /// working herdr + stale idle presence must digest.
    #[test]
    fn live_herdr_status_outranks_stale_presence() {
        let (mut host, _dir) = host_with_registry("t-herdr", entries());
        host.refresh_index();
        // Simulate the captain-pane union: PinA routed by token, not registry.
        host.captain_routed_pins.push("PinA".into());
        let line = |id: &str, ts: u64| {
            serde_json::json!({"kind": 9, "id": id, "pubkey": "ab",
                "tags": [["h","c"],["p","PinA"]],
                "content": "hi", "created_at": ts})
            .to_string()
        };
        host.tick(&BrainMsg::Presence {
            pin: "PinA".into(),
            state: "working".into(),
        });
        // Age the self-report past the freshness window: supervisor truth
        // must win from here on.
        host.presence_ts.insert("PinA".into(), 0);
        host.tick(&BrainMsg::HerdrStatus {
            pin: "PinA".into(),
            status: "idle".into(),
        });
        let fx = host.tick(&BrainMsg::Watch(line("h1", 1)));
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::Prompt { pin, .. } if pin == "PinA")),
            "live idle herdr must win over stale presence: {fx:?}"
        );
        host.tick(&BrainMsg::Presence {
            pin: "PinA".into(),
            state: "idle".into(),
        });
        host.presence_ts.insert("PinA".into(), 0);
        host.tick(&BrainMsg::HerdrStatus {
            pin: "PinA".into(),
            status: "working".into(),
        });
        let fx = host.tick(&BrainMsg::Watch(line("h2", 2)));
        assert!(
            fx.iter()
                .all(|e| !matches!(e, Effect::Prompt { pin, .. } if pin == "PinA")),
            "live working herdr must hold the prompt despite idle presence: {fx:?}"
        );
        assert!(
            fx.iter()
                .any(|e| matches!(e, Effect::Audit { kind, .. } if kind == "queued")),
            "held wake stays visible as queued: {fx:?}"
        );
    }

    #[test]
    fn fetch_inbox_normalizes_like_embedded() {
        let (mut host, _dir) = host_with_registry("t4", entries());
        let fx = host.tick(&BrainMsg::AckSlaTick { now: 1_000 });
        assert!(fx.iter().any(|e| matches!(e, Effect::CheckAckSla)));
        // Signal only: the brain fetches per-pin cursors (embedded shape).
        assert!(fx.iter().any(|e| matches!(e, Effect::FetchInbox)));
        let fx = host.tick(&BrainMsg::Sweep { now: 1 });
        assert_eq!(fx.len(), 1);
        assert!(matches!(&fx[0], Effect::FetchInbox));
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
    fn ready_bead_fact_wakes_idle_managed_pane() {
        // bd-ux-papercuts-t2u5 acceptance: a ready-bead appearance detected by
        // the beads poller must reach an idle managed pane through the daemon
        // host — the supervisor no longer has to poll the board by hand. The
        // summary comes from the real sensor, so a poller that stops emitting
        // `ready` fails this test too.
        let fresh = crate::pollers::Snapshot::default();
        let (snap, _) = crate::pollers::detect_changes(&fresh, &[], &[], "t0");
        let issue = crate::pollers::IssueRow {
            id: "bd-new".into(),
            status: "open".into(),
            updated_at: "t1".into(),
            blocked: false,
        };
        let (_snap2, delta) = crate::pollers::detect_changes(&snap, &[], &[issue], "t1");
        let summary = delta.fact_summaries().remove(0);
        assert_eq!(summary, "bead bd-new ready");

        let (mut host, _dir) = host_with_registry("t-ready", entries());
        host.refresh_index();
        host.tick(&BrainMsg::RoleBound {
            role: "captain".into(),
            pin: "PinA".into(),
        });
        host.tick(&BrainMsg::Presence {
            pin: "PinA".into(),
            state: "idle".into(),
        });
        let fx = host.tick(&BrainMsg::Fact { summary });
        assert!(
            fx.iter().any(|e| matches!(
                e,
                Effect::Prompt { pin, line } if pin == "PinA" && line == "[doorbell] bead bd-new ready"
            )),
            "ready-bead event must wake the idle captain: {fx:?}"
        );
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
    fn inbox_fail_kind_splits_pin_miss_from_path_down() {
        assert_eq!(
            super::inbox_fail_kind(
                "toron: error: tool fetch_inbox returned an error result: agent Buffy has no local key in project . (create_agent_identity or register first)"
            ),
            "inbox_pin_miss"
        );
        assert_eq!(
            super::inbox_fail_kind("unknown agent QuietHeron"),
            "inbox_pin_miss"
        );
        assert_eq!(
            super::inbox_fail_kind("toron spawn/timeout failed"),
            "inbox_error"
        );
        assert_eq!(
            super::inbox_fail_kind(
                "toron: error: daemon connect (is `toron serve --mcp-http` running?)"
            ),
            "inbox_error"
        );
        // bd-xkte: the store is AHEAD of the binary the daemon shells, so
        // every fetch refuses. The pin is fine and the fetch path is fine;
        // the reason is actionable and must not dissolve into `inbox_error`.
        assert_eq!(
            super::inbox_fail_kind(
                "toron: error: migrate: migration 14 is applied in \
                 /Users/x/.local/share/toron/index.sqlite but this toron build resolves at most \
                 13, so this binary is older than the store. Fix: rebuild or upgrade toron from \
                 the source that wrote the store (cargo install --path crates/toron-cli), or \
                 point --data-dir at a store this binary can open."
            ),
            "inbox_store_skew"
        );
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

    /// bd-68q2: the still-failing digest names the rate, the fd census, and
    /// the backoff — one row per power-of-two fails, decided by the caller.
    #[test]
    fn failing_still_row_names_rate_and_fds() {
        let row = failing_still_row(8, Some(234), 16);
        assert_eq!(
            row.get("kind").and_then(|k| k.as_str()),
            Some(AUDIT_WAKE_FAILING_STILL),
            "the row carries its kind"
        );
        assert_eq!(
            row.get("consecutive_fails").and_then(|f| f.as_u64()),
            Some(8)
        );
        assert_eq!(row.get("open_fds").and_then(|f| f.as_u64()), Some(234));
        assert_eq!(row.get("backoff_s").and_then(|b| b.as_u64()), Some(16));
        let row_none = failing_still_row(9, None, 16);
        assert_eq!(
            row_none.get("open_fds"),
            Some(&serde_json::Value::Null),
            "absent census stays null, never zero"
        );
    }

    /// bd-jqjh: a full brain channel must not silently drop HerdrStatus.
    /// The drop self-heals (self.herdr advances only on receipt, so the next
    /// sweep retries) — but the rate has to be countable and sampled-audited,
    /// same shape as wake_brain_full. Fence: no `let _ =` on this send.
    #[test]
    fn sweep_full_channel_counts_herdr_drops_and_audits_sampled() {
        let dir = tmp_state("sweep-full");
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
        // Capacity 1, prefilled: every HerdrStatus send goes Full.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(BrainMsg::Died).unwrap();
        let mut host = FileHost::new(cfg).with_tx(tx);
        host.refresh_index();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for _ in 0..4 {
                let _ = host.sweep_maintenance();
            }
        });
        assert_eq!(
            host.herdr_dropped.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "every Full send must be counted"
        );
        // The filler never left: nothing the sweep sent got through.
        assert!(matches!(rx.try_recv(), Ok(BrainMsg::Died)));
        let body = std::fs::read_to_string(dir.join("doorbell").join("w.audit.jsonl")).unwrap();
        let rows = body.matches(AUDIT_HERDR_FULL).count();
        assert_eq!(
            rows, 3,
            "sampled audit: priors 0,1,2 row, prior 3 counts only, got: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-jqjh: a closed brain channel (brain gone) counts too, flagged closed.
    #[test]
    fn sweep_closed_channel_counts_herdr_drop_as_closed() {
        let dir = tmp_state("sweep-closed");
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
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        drop(rx);
        let mut host = FileHost::new(cfg).with_tx(tx);
        host.refresh_index();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ = host.sweep_maintenance();
        });
        assert_eq!(
            host.herdr_dropped.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the Closed send must be counted"
        );
        let body = std::fs::read_to_string(dir.join("doorbell").join("w.audit.jsonl")).unwrap();
        assert!(
            body.contains(AUDIT_HERDR_FULL) && body.contains("\"closed\":true"),
            "the drop must be audited and flagged closed, got: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-jqjh: the audit door itself failing must be countable, not `let _`.
    /// A FILE where the state dir should be fails mkdir with ENOTDIR for any
    /// uid, so this stays hermetic even as root.
    #[test]
    fn audit_write_failure_increments_counter() {
        let dir = tmp_state("audit-fail");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let before = crate::store::audit_write_failures();
        FileHost::append_audit_row(&blocker, "w", "probe", serde_json::json!({"n": 1}));
        assert_eq!(
            crate::store::audit_write_failures(),
            before + 1,
            "one failed append, one count"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-ogzg: the skew is a DECLARED code from another repo, not a sentence
    /// this repo happens to agree with. Measured against two binaries and one
    /// ahead-store: the rebuilt toron says "... older than the store
    /// (toron-error=store_skew) ...", and the older installed one says the same
    /// sentence with no marker at all.
    ///
    /// The marker case is the one that carries the weight — reword the sentence
    /// around it and the class must hold; an ordinary fetch error must still
    /// never reach it.
    #[test]
    fn inbox_fail_kind_matches_the_machine_marker() {
        // Marker present, the sentence reworded: only the declared code counts.
        assert_eq!(
            super::inbox_fail_kind(
                "toron: error: migrate: this store is newer than the binary \
                 (toron-error=store_skew) Fix: rebuild or upgrade toron"
            ),
            "inbox_store_skew",
            "the declared marker alone must classify, or the structural path is untested"
        );
        // Marker absent: an installed toron older than the marker still counts.
        assert_eq!(
            super::inbox_fail_kind(
                "toron: error: migrate: migration 14 is applied in /s/index.sqlite but this \
                 toron build resolves at most 13"
            ),
            "inbox_store_skew",
            "an older shelled toron still has to classify"
        );
        // Neither: an ordinary path failure must not reach the class.
        assert_eq!(
            super::inbox_fail_kind(
                "toron: error: daemon connect (is `toron serve --mcp-http` running?)"
            ),
            "inbox_error"
        );
    }

    #[test]
    fn daemon_gen_gate() {
        let dir = tmp_state("gen-gate");
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
    /// A full channel reports ONE death, not one per line. The live burst
    /// (2026-09-28 03:53-03:56Z) was 1,928 `Died` rows
    /// in one workspace, peaking at 127 inside a single second and pulsing
    /// every few seconds for three minutes — each one claiming the child had
    /// died, each one advancing the retry backoff, and not one audit row to
    /// show for any of it.
    #[test]
    fn a_full_channel_reports_one_death_not_one_per_line() {
        let reported = std::sync::atomic::AtomicBool::new(false);
        let actions: Vec<FullChannelAction> =
            (0..200).map(|_| full_channel_action(&reported)).collect();
        let deaths = actions
            .iter()
            .filter(|action| **action == FullChannelAction::ReportDeath)
            .count();
        assert_eq!(
            deaths, 1,
            "200 un-enqueueable lines reported {deaths} deaths; every one after the first is \
             the same consumer hitting the same full channel"
        );
        assert_eq!(
            actions
                .iter()
                .filter(|a| **a == FullChannelAction::CountDropped)
                .count(),
            199,
            "the rest must be counted, not turned into deaths"
        );
    }

    /// The counted drops still leave a trace: bounded, and never silent — the
    /// flood this replaces wrote no audit row at all.
    #[test]
    fn the_drop_audit_is_geometric_not_silent() {
        let audited: Vec<usize> = (0..17).filter(|n| should_audit_count(*n)).collect();
        assert_eq!(
            audited,
            vec![0, 1, 2, 4, 8, 16],
            "a flood must leave a bounded trace: not zero rows, and not one per drop"
        );
    }

    /// The twin arm's hole: the relay had no counter and no row, so a death
    /// STORM was invisible exactly where it could have been named. 9,741 `Died`
    /// journal rows across three storms, and not one audit row about the rate.
    ///
    /// A storm is 600 deaths here, the order the live 04:32Z minute reached.
    /// The row count must stay bounded, the rows must name how many deaths they
    /// stand for, and the delivery must never become conditional — the restart
    /// depends on the death arriving, so only the audit is sampled.
    #[test]
    fn a_death_storm_leaves_a_bounded_audit_naming_its_size() {
        const DEATHS: usize = 600;
        let relayed: Vec<usize> = (0..DEATHS)
            .filter_map(|prior| match died_relay_action(prior) {
                DiedRelayAction::RelayAndAudit(n) => Some(n),
                DiedRelayAction::RelayOnly => None,
            })
            .collect();
        assert_eq!(
            relayed,
            vec![1, 2, 3, 5, 9, 17, 33, 65, 129, 257, 513],
            "600 deaths must leave ~log2 rows, the first naming itself and each row \
             naming the deaths it stands for"
        );
        assert_eq!(
            relayed.last(),
            Some(&513),
            "the newest row must account for the run, not just say 'a death happened'"
        );
    }

    /// bd-lorot: a burst of deaths must schedule ONE restart.
    ///
    /// This is the fence for the live incident of 2026-09-27, where the
    /// doorbell wrote 1,613 `wake_sock_absent` audit rows in one second against
    /// a 16s backoff. The cause was not a slow backoff — it was that a backoff
    /// spaces ONE retry, and nothing stopped a burst of `Died` from each
    /// scheduling its own timer. 1,613 timers fired together, connected 1,613
    /// times, and drove the toron daemon to EMFILE, killing the socket the
    /// doorbell was polling.
    ///
    /// `retry.fails` is the witness: `restart_wake` calls `record_fail()` only
    /// when a restart is actually SCHEDULED, so the counter is a direct read on
    /// how many got past the guard. Neuter `restart_pending` and this asserts
    /// 1 while the code produces 51.
    #[test]
    fn a_burst_of_deaths_schedules_exactly_one_restart() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let dir = tmp_state("burst");
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = "true".into();
        cfg.herdr_bin = "true".into();
        let mut host = FileHost::new(cfg);
        host.tx = Some(tokio::sync::mpsc::channel(64).0);

        rt.block_on(async {
            // bd-watch-restart-metric-8dvf: the gate's own answer IS the
            // honest metric (the brain journals a restart row only for `true`).
            // One death restarted; the other 50 coalesced and must report that
            // they did not, or the journal counts deaths.
            let mut took_the_slot = 0;
            if host.restart_wake() {
                took_the_slot += 1;
            }
            for _ in 0..50 {
                if host.restart_wake() {
                    took_the_slot += 1;
                }
            }
            // Let the single scheduled task reach its sleep; the burst is
            // coalesced during the sleep, which is the window that matters.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                took_the_slot, 1,
                "51 deaths reported {took_the_slot} restarts; the journal counts this answer, \
                 so a gate that let each death through would count deaths as restarts"
            );
        });

        let fails = host.retry.lock().expect("gate").fails();
        assert_eq!(
            fails, 1,
            "51 deaths scheduled {fails} restarts: without single-flight every death brings \
             its own timer and they all fire together after the sleep, which is how 1,613 \
             connects landed in one second"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-lorot: and the gate must REOPEN, or single-flight would become a
    /// one-shot and a later outage would never be retried at all. The flag is
    /// cleared when the scheduled task is about to act.
    #[test]
    fn the_single_flight_flag_reopens_after_the_task_fires() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let dir = tmp_state("reopen");
        let mut cfg = FileHostConfig::new(dir.clone(), "proj", "w");
        cfg.toron_bin = "true".into();
        cfg.herdr_bin = "true".into();
        let mut host = FileHost::new(cfg);
        host.tx = Some(tokio::sync::mpsc::channel(64).0);

        rt.block_on(async {
            host.restart_wake();
            // The first backoff is 1s, so wait past it: the flag clears as the
            // task wakes, and the NEXT death must be schedulable again.
            tokio::time::sleep(std::time::Duration::from_millis(1_400)).await;
            host.restart_wake();
        });

        let fails = host.retry.lock().expect("gate").fails();
        assert!(
            fails >= 2,
            "a death after the first task fired scheduled nothing (fails={fails}): the flag \
             never reopened, so this would be a one-shot and the second outage would be \
             never retried — silence instead of a storm"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// bd-lorot: bounded audit log.
//
// The journal has a full retention discipline (512 KiB rotate gate,
// RETENTION_LINES 5000, RETENTION_GENS 4, RETENTION_BYTES 5 MiB) and the AUDIT
// log had NONE — it was a bare `OpenOptions::append` + `writeln!`. That is
// exactly backwards: the audit log records every PER-ATTEMPT failure, so it is
// the high-volume file, and it is what a repeating fault writes to. Measured
// live (bd-kz54): one stale-socket respawn loop grew `flywheel-toron` to 14 GB
// and the whole doorbell state dir to 26 GB — 65.5 M rows of one identical
// failure, none of which said anything the first row did not.
//
// The journal is allowed to keep generations because it is the run record. The
// audit log is diagnostics, so it is capped in place: keep the newest rows,
// drop the rest. Retention is by BYTES as well as rows, because a pathological
// row (a long error string) can blow a row budget on its own.
// ---------------------------------------------------------------------------

/// Hard cap on one workspace's audit log.
pub const AUDIT_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Newest rows kept on rotation. Matches the journal's `KEEP_LINES` so both logs
/// age out on the same scale.
pub const AUDIT_KEEP_LINES: usize = 4000;

// bd-iyq5: a fence, not a test. Both sides are compile-time constants, so a
// runtime assertion checked nothing a build would not — a keep-count of zero, or
// one too big to fit the cap, must stop the BUILD rather than a test run.
const _: () = {
    assert!(
        AUDIT_KEEP_LINES > 0,
        "rotation that keeps nothing erases the log"
    );
    assert!(
        AUDIT_KEEP_LINES * 250 < AUDIT_MAX_BYTES as usize,
        "the rows kept must fit inside the byte cap"
    );
};

/// Pure: the newest `keep` complete lines of an audit body, each newline
/// terminated. A rotation must never leave a half-written row, because the next
/// append concatenates onto it and corrupts one record.
pub fn audit_tail_keep(body: &str, keep: usize) -> String {
    if keep == 0 {
        return String::new();
    }
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.len().saturating_sub(keep);
    let mut out = String::new();
    for l in &lines[start..] {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// Cap the audit log, reading only its TAIL.
///
/// Reading the whole file would OOM the daemon — the file this exists to bound
/// was measured at 14 GB. So seek to `len - AUDIT_MAX_BYTES`, drop the partial
/// first line the seek landed inside, keep the newest rows, and swap via a temp
/// file + rename so a crash mid-rotation leaves the original intact rather than
/// a truncated one.
fn rotate_audit_if_oversized(path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(md) = std::fs::metadata(path) else {
        return;
    };
    if md.len() <= AUDIT_MAX_BYTES {
        return;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return;
    };
    let start = md.len().saturating_sub(AUDIT_MAX_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return;
    }
    // The seek almost certainly landed mid-row; drop through the first newline.
    let body = match buf.find('\n') {
        Some(i) => &buf[i + 1..],
        None => buf.as_str(),
    };
    let keep = audit_tail_keep(body, AUDIT_KEEP_LINES);
    let tmp = path.with_extension("jsonl.rotating");
    if std::fs::write(&tmp, keep).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod audit_rotation_tests {
    use super::{AUDIT_MAX_BYTES, audit_tail_keep, rotate_audit_if_oversized};
    use std::io::Write;

    /// The tail is the NEWEST rows, never the oldest — an audit log whose
    /// rotation keeps the beginning is useless for diagnosing a live fault.
    #[test]
    fn rotation_keeps_the_newest_rows_and_terminates_every_line() {
        let body: String = (0..100).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        let kept = audit_tail_keep(&body, 10);
        let lines: Vec<&str> = kept.lines().collect();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "{\"n\":90}");
        assert_eq!(lines[9], "{\"n\":99}");
        assert!(
            kept.ends_with('\n'),
            "a partial tail row corrupts the next append"
        );
    }

    #[test]
    fn keeping_more_than_exists_returns_everything() {
        let body = "{\"n\":1}\n{\"n\":2}\n";
        assert_eq!(audit_tail_keep(body, 500), body);
    }

    #[test]
    fn keeping_nothing_returns_nothing() {
        assert_eq!(audit_tail_keep("{\"n\":1}\n", 0), "");
    }

    /// The real defect, at real scale (past the 5 MiB cap) so the tail-only read
    /// is the path that actually runs.
    #[test]
    fn an_oversized_audit_file_is_bounded_and_keeps_the_recent_tail() {
        let dir = std::env::temp_dir().join(format!("audit-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("ws.audit.jsonl");

        let mut f = std::fs::File::create(&path).expect("create");
        for i in 0..40_000u64 {
            let row = format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "x".repeat(120));
            f.write_all(row.as_bytes()).expect("write");
        }
        f.flush().expect("flush");
        drop(f);
        let before = std::fs::metadata(&path).expect("meta").len();
        assert!(
            before > AUDIT_MAX_BYTES,
            "premise: the fixture must exceed the cap, got {before}"
        );

        rotate_audit_if_oversized(&path);
        let after = std::fs::metadata(&path).expect("meta").len();
        assert!(
            after <= AUDIT_MAX_BYTES,
            "rotation must bound the file, got {after} (was {before})"
        );
        let body = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = body.lines().collect();
        assert!(!lines.is_empty(), "rotation must not empty the log");
        assert!(
            lines[0].starts_with("{\"n\":") && lines[0].contains("pad"),
            "the first retained row must be a WHOLE row, not a fragment: {}",
            &lines[0][..lines[0].len().min(60)]
        );
        assert!(
            body.contains("\"n\":39999"),
            "the newest row must survive — a rotation that keeps the oldest is useless"
        );
        assert!(
            !std::path::Path::new(&format!("{}.rotating", path.display())).exists(),
            "the temp file must not be left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Under the cap nothing happens — the cheap gate must not rewrite a healthy
    /// log on every append.
    #[test]
    fn a_healthy_audit_file_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("audit-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("ws.audit.jsonl");
        let body: String = (0..50).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        std::fs::write(&path, &body).expect("write");
        rotate_audit_if_oversized(&path);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The keep-count fence moved to a `const` assertion beside the constants
    // (bd-iyq5). Both sides were compile-time constants, so a `#[test]` that
    // asserted them checked nothing a build would not — and clippy was right to
    // say so. The fence survives; it just fails the BUILD now instead of a test
    // run.
}

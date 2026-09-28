// governed-by: ADR-0012
//! Serve loop (D2): UDS + TCP bind, hand-rolled `/health`, hourly 1 MiB
//! copy-truncate logrotate. No HTTP framework: one GET route needs
//! sixty lines, not a dependency.
//!
//! Phase 3 (bd-ene2): the daemon hosts one [`Brain`](doorbell_rt::Brain)
//! per configured `--host project/workspace`, driven by the daemon-native
//! [`FileHost`](doorbell_rt::FileHost) (file/CLI inputs, no flywheel
//! types). Journals land in the flywheel state dir, shared with
//! `flywheel doorbell why/digest/afk`. No `--host` = standby (bind +
//! reads only), reported honestly in `/health`.

use crate::lifecycle::Config;
use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LOGROTATE_BYTES: u64 = 1024 * 1024;

/// One hosted loop: watch child + sweep/ack beats around a Brain. The
/// channel comes from `run` so shutdown (drop all senders) ends the
/// brain cleanly with drained, journaled intents.
async fn host_workspace(
    fw_state: std::path::PathBuf,
    project: String,
    workspace: String,
    tx: tokio::sync::mpsc::Sender<doorbell_rt::BrainMsg>,
    rx: tokio::sync::mpsc::Receiver<doorbell_rt::BrainMsg>,
    liveness: std::sync::Arc<doorbell_rt::brain::BrainLiveness>,
) -> doorbell_rt::RunReport {
    use doorbell_rt::{Brain, BrainConfig, FileHost, FileHostConfig, Host, SinkManager};
    use doorbell_rt::{CliPromptSink, FileStore};
    let bins = resolved_bins();
    let mut fcfg = FileHostConfig::new(fw_state.clone(), project.clone(), workspace.clone());
    fcfg.toron_bin = bins.toron.clone();
    fcfg.herdr_bin = bins.herdr.clone();
    let mut host = FileHost::new(fcfg).with_tx(tx.clone());
    // Daemon-keyed generation: first run captures the doorbell binary
    // identity so a mid-run upgrade retires the loop instead of waking
    // stale (same rule as the embedded WatcherGen gate, daemon-keyed).
    doorbell_rt::ensure_daemon_gen(&fw_state, &workspace);
    let herdr_bin = bins.herdr.clone();
    let mut sinks = SinkManager::new(tx.clone());
    // Supervised registration (audit P1): a failed herdr CLI park gets a
    // fresh adapter under the Transient policy instead of dying until
    // process restart. Targets table is shared, so the factory sees the
    // refreshed routing after registry changes.
    let targets = host.target_table();
    sinks.add_sink_with_factory(
        "herdr",
        Box::new(CliPromptSink::new(herdr_bin.clone(), targets.clone())),
        move || Box::new(CliPromptSink::new(herdr_bin.clone(), targets.clone())),
    );
    // bd-8ulu: a durable human wake with zero swarm needs somewhere to land.
    // The pane registry is not an option for a human pin, so register the
    // webhook adapters this workspace's rows justify — one per distinct
    // endpoint, allowlist-gated. A row that cannot be honoured registers
    // nothing and is named LOUDLY here: silence would look like a delivered
    // wake while the digest quietly absorbs it.
    let allowlist = doorbell_rt::sinks::EgressAllowlist::from_env();
    let (hook_regs, refused) = webhook_registrations(
        &doorbell_rt::subscribe::SubscriptionStore::open(&fw_state).list(),
        &workspace,
        &allowlist,
    );
    for line in &refused {
        eprintln!("doorbell serve: {project}/{workspace} — {line}");
    }
    for reg in hook_regs {
        match doorbell_rt::sinks::WebhookSink::new(reg.url.clone(), &allowlist, false) {
            Ok(hook) => {
                sinks.add_sink(reg.route.clone(), Box::new(hook));
                eprintln!(
                    "doorbell serve: hosted loop {project}/{workspace} — registered {} (endpoint {})",
                    reg.route, reg.url
                );
            }
            Err(e) => eprintln!(
                "doorbell serve: {project}/{workspace} — {} NOT registered: {e}; its wakes \
                 stay digest-visible",
                reg.route
            ),
        }
    }
    let store = FileStore::new(fw_state);
    // Wake-sock consumer (the in-process bus needs toron-core, which the
    // lean daemon never links). Backoff + retry gate live in the host.
    host.restart_wake();
    // Beats: 1s sweep, 60s ack-SLA (same cadences as embedded).
    let sweep_ticker = doorbell_rt::interval_ticker(
        "sweep",
        tx.clone(),
        std::time::Duration::from_secs(1),
        || doorbell_rt::BrainMsg::Sweep {
            now: doorbell_rt::lease::unix_now(),
        },
    );
    let _sla_ticker = doorbell_rt::interval_ticker(
        "ack-sla",
        tx.clone(),
        std::time::Duration::from_secs(60),
        || doorbell_rt::BrainMsg::AckSlaTick {
            now: doorbell_rt::lease::unix_now(),
        },
    );
    let brain =
        Brain::new(BrainConfig::new(&workspace), host, store, sinks).with_liveness(liveness);
    let report = brain.run(rx).await;
    drop(sweep_ticker);
    eprintln!(
        "doorbell serve: hosted loop {project}/{workspace} exited (injects={} sheds={} degraded={})",
        report.stats.injects, report.stats.sheds, report.stats.degraded
    );
    report
}
/// Shared read state for the HTTP/UDS handlers.
#[derive(Clone)]
struct AppState {
    started: std::time::Instant,
    fw_state: std::path::PathBuf,
    /// bd-2o1v: hosting is dynamic (auto-host on subscribe), so the table
    /// is shared mutably; handlers snapshot the keys under the lock.
    hosted: Hosts,
    /// bd-o86s: the served /health snapshot. `health_body` shells herdr and
    /// greps hundreds of journal/audit files per call (seconds at fleet
    /// scale), so handlers serve this cache and a background tick refreshes
    /// it — never compute per request.
    health: Shared<CachedHealth>,
}

/// One served /health snapshot: the body as computed plus when. Empty body
/// means uncomputed (cold start serves one inline compute, then the tick).
struct CachedHealth {
    body: String,
    computed_at: std::time::Instant,
}

/// Freshness bound on the served snapshot (bd-o86s): sweep ages read in
/// whole seconds, so a snapshot up to this old never lies about liveness,
/// while every served request stays far under the probe budget.
const HEALTH_CACHE_TTL_SECS: u64 = 15;

/// bd-2o1v: shared mutable state handle (parking_lot — no poisoning).
type Shared<T> = std::sync::Arc<parking_lot::Mutex<T>>;

/// One hosted loop's control handle: its `project/workspace` key and the
/// sender that shuts its Brain down.
///
/// Keyed by construction (bd-mk60). The daemon used to keep three parallel
/// `Vec`s (keys / handles / senders), which could not name WHICH loop to stop,
/// so reconcile could add hosts but never retire one: unsubscribing a
/// workspace left its Brain sweeping forever and holding a writer lease.
struct HostedLoop {
    key: String,
    tx: tokio::sync::mpsc::Sender<doorbell_rt::BrainMsg>,
}

type Hosts = Shared<Vec<HostedLoop>>;

/// One hosted loop's supervised task, keyed like `HostedLoop`: bd-mk60's
/// lesson applied to the drain (bd-zzgb). An anonymous
/// `Vec<JoinHandle>` cannot name WHICH loop missed the deadline, so a
/// budget bail reported only a count and pinned nothing — and awaiting
/// those handles head-of-line let one hung Brain claim every other loop
/// had failed too.
struct LoopEntry {
    key: String,
    task: tokio::task::JoinHandle<()>,
    /// bd-y40k: the shared probe this brain's task touches — read at the
    /// drain deadline for the straggler's starve/churn/lost-wake verdict.
    liveness: Option<std::sync::Arc<doorbell_rt::brain::BrainLiveness>>,
}

/// Second sample width for the growth test (bd-y40k): short enough to sit
/// inside a diagnostic path nobody waits on in the happy case, long enough
/// for a churning brain to log another wake.
pub(crate) const LIVENESS_PROBE_SAMPLE: std::time::Duration = std::time::Duration::from_millis(200);

type Loops = Shared<Vec<LoopEntry>>;

/// Hosted keys in insertion order (reconcile / stats bookkeeping).
/// `/health` uses [`hosted_keys_for_health`] (GS-062 alive-first slug order).
fn hosted_keys(hosts: &[HostedLoop]) -> Vec<String> {
    hosts.iter().map(|h| h.key.clone()).collect()
}

/// GS-062: `/health` hosted order — alive-first then alphabetical **workspace
/// slug** (shared with flywheel serve via
/// [`doorbell_rt::wake_liveness::order_hosted_alive_first_by`]).
///
/// In-memory [`HostedLoop`]s are all live in this process, so the alive
/// partition holds every key. Sort by the workspace half of
/// `project/workspace` (not insertion order) so primary converges with
/// flywheel's slug order.
fn hosted_keys_for_health(hosts: &[HostedLoop]) -> Vec<String> {
    let alive: Vec<String> = hosts.iter().map(|h| h.key.clone()).collect();
    doorbell_rt::wake_liveness::order_hosted_alive_first_by(alive, Vec::new(), |k| {
        hosted_workspace(k).unwrap_or(k)
    })
}

/// The external CLIs this daemon execs. Resolved through the shared search
/// order (`doorbell_rt::binpath`) — under launchd/systemd `PATH` is
/// `/usr/bin:/bin:/usr/sbin:/sbin`, which holds neither `toron` nor
/// `herdr`, so a bare name silently breaks the watch child and every
/// prompt while the daemon still advertises `mode: hosted`.
/// `*_resolved=false` keeps the bare name as a last resort (the spawn then
/// fails visibly) and is reported as `degraded` in `/health`.
#[derive(Clone)]
struct Bins {
    toron: String,
    herdr: String,
    toron_resolved: bool,
    herdr_resolved: bool,
}

fn resolved_bins() -> Bins {
    use doorbell_rt::binpath::{HERDR_BIN_ENV, TORON_BIN_ENV, resolve_cli_or_name};
    let (toron, toron_resolved) = resolve_cli_or_name("toron", TORON_BIN_ENV);
    let (herdr, herdr_resolved) = resolve_cli_or_name("herdr", HERDR_BIN_ENV);
    Bins {
        toron: toron.to_string_lossy().into_owned(),
        herdr: herdr.to_string_lossy().into_owned(),
        toron_resolved,
        herdr_resolved,
    }
}

/// A degradation probe is about NOW, not history: a fetch error older than
/// this is stale evidence, not a live fault.
const PROBE_FRESH_SECS: u64 = 120;

/// True only when every hosted workspace has a fresh path-error. Empty
/// host set is standby, not a fetch outage.
fn inbox_path_down(ages: &[Option<u64>], fresh_secs: u64) -> bool {
    !ages.is_empty() && ages.iter().all(|a| a.is_some_and(|age| age <= fresh_secs))
}

/// Watch-thrash measurement window. A healthy quiet watcher idle-exits at
/// 90s and is respawned after the retry gate (~109s cadence measured live),
/// so individual restarts are routine and two workspaces phase them
/// independently — only the *frequency* separates thrash (live: every 16s)
/// from health. Allowance is ~2x the healthy cadence.
const WATCH_WINDOW_SECS: u64 = 300;
/// bd-lorot: a journal for a workspace this daemon does not host is only
/// abandoned after this long. Generous on purpose — deleting a journal is not
/// reversible, and the failure mode of being too eager is a live crew losing its
/// wake record, while being too late only costs a file.
const UNHOSTED_JOURNAL_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
const WATCH_RESTARTS_ALLOWED_PER_WS: usize = 6;

fn workspace_audit_path(fw_state: &std::path::Path, ws: &str) -> std::path::PathBuf {
    fw_state.join("doorbell").join(format!("{ws}.audit.jsonl"))
}

fn workspace_journal_path(fw_state: &std::path::Path, ws: &str) -> std::path::PathBuf {
    fw_state
        .join("journals")
        .join(format!("doorbell-journal-{ws}.jsonl"))
}

/// Reap threshold for orphaned spawn-scoped subscriptions (bd-2o1v): a
/// crashed fleet's rows die after a week of journal silence. `kill`
/// normally drops its own rows; this is the belt.
const REAP_AFTER_SECS: u64 = 7 * 24 * 3600;

/// Every `project/workspace` key named by a subscription row. Legacy rows
/// without a project name nothing (never guessed into a host).
///
/// The single key builder: auto-host ([`unhosted_subscribed`]) and retire
/// ([`retire_keys`]) must agree on what a subscription references, or a loop
/// could be spawned by one rule and retired by another.
fn hostable_project(project: &str) -> bool {
    let p = project.trim();
    !p.is_empty() && p != "." && p != ".."
}

fn subscribed_keys(subs: &[doorbell_rt::subscribe::Subscription]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for sub in subs {
        let Some(project) = &sub.project else {
            continue;
        };
        // `.` / `..` / blank are cwd aliases, not slugs. Auto-hosting them
        // sweeps unregistered pins (Buffy on project `.`) and the resulting
        // inbox_error paints the whole daemon fetch-path-down (bd-gpj0.1).
        if !hostable_project(project) {
            continue;
        }
        for ws in sub.scope.workspaces() {
            let key = format!("{project}/{ws}");
            if !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

/// One webhook adapter a hosted workspace must register (bd-8ulu).
#[derive(Debug, Clone, PartialEq, Eq)]
struct WebhookReg {
    /// Adapter name the wake routes to (`webhook:<url>`).
    route: String,
    url: String,
}

/// bd-8ulu: which webhook adapters this workspace's subscription rows
/// justify. Pure over `(rows, allowlist)` so the decision is testable
/// without a runtime, a socket, or a daemon.
///
/// Returns `(registrations, refusals)`. `refusals` are operator-facing lines,
/// not errors: a row that cannot be honoured registers NOTHING and its wake
/// stays digest-visible — the fail-closed direction (bd-bk9t's rule: never
/// manufacture a `Delivered` receipt for a wake nobody received).
fn webhook_registrations(
    subs: &[doorbell_rt::subscribe::Subscription],
    workspace: &str,
    allowlist: &doorbell_rt::sinks::EgressAllowlist,
) -> (Vec<WebhookReg>, Vec<String>) {
    use doorbell_rt::subscribe::{Identity, SinkKind};
    let mut regs: Vec<WebhookReg> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for sub in subs {
        if sub.sink != SinkKind::Webhook || !sub.scope.workspaces().iter().any(|w| w == workspace) {
            continue;
        }
        let who = match &sub.identity {
            Identity::Pin(p) => p.clone(),
            other => other.tag(),
        };
        let Some(url) = sub
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        else {
            refused.push(format!(
                "webhook subscription {} ({who}) names no --url: nothing registered, \
                 its wakes stay digest-visible",
                sub.id
            ));
            continue;
        };
        if !allowlist.allows(url) {
            refused.push(format!(
                "webhook subscription {} ({who}) host not on {}: refused (D6 egress), \
                 its wakes stay digest-visible",
                sub.id,
                doorbell_rt::sinks::EGRESS_ALLOWLIST_ENV
            ));
            continue;
        }
        let route = sub.route_name();
        if !regs.iter().any(|r| r.route == route) {
            regs.push(WebhookReg {
                route,
                url: url.to_string(),
            });
        }
    }
    (regs, refused)
}

/// (project, workspace) pairs named by subscriptions but not yet hosted.
/// Legacy rows without a project are never guessed into a host.
fn unhosted_subscribed(
    subs: &[doorbell_rt::subscribe::Subscription],
    hosted: &[String],
) -> Vec<(String, String)> {
    subscribed_keys(subs)
        .into_iter()
        .filter(|key| !hosted.contains(key))
        .filter_map(|key| {
            key.rsplit_once('/')
                .map(|(p, w)| (p.to_string(), w.to_string()))
        })
        .collect()
}

/// Which hosted keys reconcile should retire (bd-mk60).
///
/// Retire only when BOTH hold: the loop is not boot-configured — `--host` is
/// an explicit operator choice that outlives any subscription — and no
/// subscription row references it any more. Boot config is compared as a key
/// so a boot host is never retired while it is the only reason the plane runs.
fn retire_keys(hosted: &[String], boot: &[String], live: &[String]) -> Vec<String> {
    hosted
        .iter()
        .filter(|k| !boot.contains(k) && !live.contains(k))
        .cloned()
        .collect()
}

/// Spawn-scoped rows whose workspace journal has been silent past
/// [`REAP_AFTER_SECS`]. No journal yet (fresh spawn) or a live journal
/// keeps the row; operator/cli rows always stay.
fn reap_candidates(
    subs: &[doorbell_rt::subscribe::Subscription],
    journal_age_secs: impl Fn(&str) -> Option<u64>,
) -> Vec<String> {
    subs.iter()
        .filter(|s| s.id.starts_with("spawn-"))
        .filter_map(|s| {
            let ws = s.scope.workspaces().into_iter().next()?;
            match journal_age_secs(&ws) {
                Some(age) if age > REAP_AFTER_SECS => Some(s.id.clone()),
                _ => None,
            }
        })
        .collect()
}

/// Mtime age of a workspace's journal file, in seconds (None = no journal).
fn journal_age_secs(fw_state: &std::path::Path, ws: &str) -> Option<u64> {
    let md = std::fs::metadata(
        fw_state
            .join("journals")
            .join(format!("doorbell-journal-{ws}.jsonl")),
    )
    .ok()?;
    let modified = md.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(doorbell_rt::lease::unix_now().saturating_sub(secs))
}

/// SIGTERM drain budget. `doorbell stop` allows 20s before escalating to
/// SIGKILL, so the drain must always finish inside this: a Brain that
/// never joins used to keep the pid alive until stop misreported    /// "ignored SIGTERM" and aborted the restart (bd-xwgh round 2).
pub(crate) const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// The send phase's own bound (bd-umuh): `Shutdown` sends rode ahead of
/// the drain with no timeout of their own — one brain not receiving
/// parked the serial walk, the 10s drain budget never started, and
/// stop's 20s grace expired mid-drain (SIGKILL before any note printed).
/// Healthy sends enqueue in microseconds; 2s only ever trips a brain
/// that never recv()s. Stop math: SEND + DRAIN + probe + exit = 17.2s
/// ≤ STOP_GRACE 20s (asserted in `stop_grace_covers_...`).
pub(crate) const SEND_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Pure note for the drain outcome — a budget exhaustion must never read
/// as a clean join (scripted in tests; hang case included).
fn drain_note(joined: bool, remaining: usize, total: usize) -> String {
    if joined {
        "doorbell serve: loops joined, releasing leases".to_string()
    } else {
        format!(
            "doorbell serve: DRAIN BUDGET EXCEEDED ({}s) — {remaining} of {total} loop(s) did not join; releasing leases and exiting",
            DRAIN_BUDGET.as_secs()
        )
    }
}

/// What one drain learned: whether the budget held, how many loops were
/// still running at the deadline, and WHICH keys they were (bd-zzgb — an
/// anonymous count could not pin the loop that missed the drain).
#[derive(Debug, PartialEq)]
struct DrainReport {
    joined: bool,
    remaining: usize,
    total: usize,
    stragglers: Vec<String>,
    /// bd-y40k: (key, verdict) per straggler probed at the deadline —
    /// a diagnosis nobody prints pins nothing.
    verdicts: Vec<(String, String)>,
}
/// Why a `Shutdown` send failed — observation first, inference never
/// (bd-925m): `ChannelClosed` is proof the receiver is gone; a budget
/// exhaustion only proves the enqueue did not finish in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendFail {
    ChannelClosed,
    BudgetExhausted,
}

impl std::fmt::Display for SendFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendFail::ChannelClosed => write!(f, "channel closed"),
            SendFail::BudgetExhausted => write!(f, "send budget exhausted"),
        }
    }
}

/// Send `Shutdown` to every retiring loop under ONE shared deadline,
/// returning each failure as a VALUE with its reason — the caller is
/// loud (bd-burk: closed channels; bd-umuh: a bounded budget).
///
/// bd-v5r0: the sends RACE, which is the same fix the join phase below
/// already carries. The walk used to be sequential under a single
/// deadline, so the first channel that was full — a brain mid-dispatch —
/// parked it for the whole budget and every key BEHIND it was reported
/// BudgetExhausted without ever being tried. Live on the 2026-09-28 restart
/// that was all three hosted workspaces at once, so all three brains died
/// with the process instead of draining their sinks. Each send now races
/// the deadline on its own: a key is BudgetExhausted only if its OWN send
/// did not land, and the total wall time is still bounded by `budget`
/// (stop's grace covers it, asserted in `stop_grace_covers_...`).
/// Cancellation-safe: a dropped `send` future enqueues nothing, and
/// `join_all` preserves table order, so the same table reports the same way
/// twice.
async fn send_shutdowns(
    retiring: &[HostedLoop],
    budget: std::time::Duration,
) -> Vec<(String, SendFail)> {
    let deadline = tokio::time::Instant::now() + budget;
    let sends = retiring.iter().map(|h| async move {
        match tokio::time::timeout_at(deadline, h.tx.send(doorbell_rt::BrainMsg::Shutdown)).await {
            Ok(Ok(())) => None,
            Ok(Err(_)) => Some((h.key.clone(), SendFail::ChannelClosed)),
            Err(_) => Some((h.key.clone(), SendFail::BudgetExhausted)),
        }
    });
    futures::future::join_all(sends)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Join every supervised loop CONCURRENTLY inside `budget` (bd-zzgb).
/// The old drain awaited handles in order, so a single hung brain stalled
/// the whole scan: loops that finished behind it were never observed and
/// the note claimed they all missed the deadline ("6 of 6"). All handles
/// race the budget together; stragglers are read from `is_finished` AFTER
/// the timeout, never from a decrement counter. A JoinError is still
/// surfaced loudly per key — a panicked Brain is a defect, not a clean
/// join. At the deadline the join future is dropped; the handles stay
/// owned by `entries` (only the borrow ends), so nothing is cancelled
/// mid-write — the exiting process is what stops the stragglers.
async fn drain_loops(entries: &mut [LoopEntry], budget: std::time::Duration) -> DrainReport {
    let total = entries.len();
    let joins: Vec<&mut tokio::task::JoinHandle<()>> =
        entries.iter_mut().map(|e| &mut e.task).collect();
    match tokio::time::timeout(budget, futures::future::join_all(joins)).await {
        Ok(done) => {
            for (entry, res) in entries.iter().zip(done) {
                if let Err(e) = res {
                    eprintln!(
                        "doorbell serve: hosted loop {} task ended abnormally ({e})",
                        entry.key
                    );
                }
            }
            DrainReport {
                joined: true,
                remaining: 0,
                total,
                stragglers: Vec::new(),
                verdicts: Vec::new(),
            }
        }
        Err(_elapsed) => {
            let stragglers: Vec<String> = entries
                .iter()
                .filter(|e| !e.task.is_finished())
                .map(|e| e.key.clone())
                .collect();
            let remaining = stragglers.len();
            // bd-y40k verdicts, bd-pf3b timing: sample EVERY straggler first
            // (age + wake count at the deadline), then ONE shared window —
            // a serial sleep per key stacked N×200ms onto the drain and
            // pushed total shutdown past stop's grace under load. Growth is
            // read across the single window; age is the deadline-age (the
            // honest "last wake N ms before the probe").
            let mut samples: Vec<(
                String,
                std::sync::Arc<doorbell_rt::brain::BrainLiveness>,
                u64,
                u64,
            )> = Vec::new();
            for entry in entries.iter().filter(|e| !e.task.is_finished()) {
                if let Some(liv) = entry.liveness.as_ref() {
                    samples.push((
                        entry.key.clone(),
                        liv.clone(),
                        liv.wakes(),
                        liv.age_ms(tokio::time::Instant::now()),
                    ));
                }
            }
            if !samples.is_empty() {
                tokio::time::sleep(LIVENESS_PROBE_SAMPLE).await;
            }
            let mut verdicts = Vec::new();
            for (key, liv, wakes_before, age_at_deadline) in samples {
                let grew = liv.wakes() > wakes_before;
                verdicts.push((
                    key,
                    doorbell_rt::brain::classify_liveness(age_at_deadline, grew).describe(),
                ));
            }
            DrainReport {
                joined: false,
                remaining,
                total,
                stragglers,
                verdicts,
            }
        }
    }
}

/// Names the loops that outlived the drain budget — the live pin for
/// bd-zzgb's next occurrence. None when everything joined.
fn straggler_note(report: &DrainReport) -> Option<String> {
    if report.stragglers.is_empty() {
        return None;
    }
    let mut note = format!(
        "doorbell serve: straggler loop(s) still running after {}s: {}",
        DRAIN_BUDGET.as_secs(),
        report.stragglers.join(", ")
    );
    // bd-y40k: the mechanism, named per key — starved / churning / lost-wake.
    for (key, verdict) in &report.verdicts {
        note.push_str(&format!("\n  {key}: {verdict}"));
    }
    Some(note)
}

pub async fn run(cfg: &Config) -> Result<()> {
    use doorbell_rt::lease;
    let now = lease::unix_now();
    // D9 layer 1 first, per workspace: a live lease means another writer
    // runs there — refuse before touching its socket (never clobber a
    // stranger's bind). Leases live in the FLYWHEEL state dir, shared
    // with embedded loops, so either side excludes the other per ws.
    for (project, workspace) in &cfg.hosts {
        if let Some(cur) = lease::read_lease(&cfg.flywheel_state, workspace)
            && !lease::lease_stale(&cur, now)
        {
            anyhow::bail!(
                "doorbell serve: refused, live lease for {project}/{workspace} held by pid {} on {} (started {})",
                cur.pid,
                cur.host,
                cur.started_unix
            );
        }
    }
    let tcp = match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", cfg.port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // D9 layer 2, conflict-class: EVERY arm exits 0 so the unit's
            // KeepAlive{SuccessfulExit=false} never respawn-wars against a
            // holder that will not move. The old exit-1 "squatter" bail is
            // what produced runs=69 / watch_restart_loop whenever a
            // mid-shutdown doorbell merely failed to answer in time.
            match classify_port_holder(peer_health(cfg).await.as_ref()) {
                PortHolder::Peer => eprintln!(
                    "doorbell serve: healthy doorbell peer on {}, exiting 0",
                    cfg.port
                ),
                PortHolder::Unresponsive => eprintln!(
                    "doorbell serve: port {} accepts connections but never answers /health \
                     (a daemon mid-shutdown, or a wedged process) — exiting 0 (stay dead; \
                     inspect with lsof -nP -iTCP:{} then stop/retry)",
                    cfg.port, cfg.port
                ),
                PortHolder::Foreign => eprintln!(
                    "doorbell serve: port {} answers with a non-doorbell protocol — foreign \
                     squatter, exiting 0 (stay dead; free the port then start again)",
                    cfg.port
                ),
            }
            std::process::exit(0);
        }
        Err(e) => return Err(e).with_context(|| format!("bind 127.0.0.1:{}", cfg.port)),
    };
    // Claim one writer lease per hosted workspace before serving.
    let owned: Shared<Vec<(String, doorbell_rt::lease::OwnerLease)>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    for (_, workspace) in &cfg.hosts {
        match lease::acquire_lease(&cfg.flywheel_state, workspace, lease::unix_now()) {
            Ok(l) => owned.lock().push((workspace.clone(), l)),
            Err(e) => {
                for (ws, l) in owned.lock().iter() {
                    lease::release_lease(&cfg.flywheel_state, ws, l);
                }
                anyhow::bail!("doorbell serve: lease claim failed after bind: {e:?}");
            }
        }
    }
    let sock_path = cfg.socket_path();
    if let Some(parent) = sock_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            parent_of(&sock_path),
            std::fs::Permissions::from_mode(0o700),
        );
        let _ = std::fs::remove_file(&sock_path);
    }
    let uds = tokio::net::UnixListener::bind(&sock_path)
        .with_context(|| format!("bind {}", sock_path.display()))?;
    // Pidfile claim lives with the bind owner (bd-start-adopt): `serve`
    // claims it once both binds succeed, so a launchd/systemd-started
    // daemon is pidfile-managed identically to a `start` child and
    // `doorbell start` adopts instead of dying on a foreign-owner fence.
    if let Err(e) = crate::lifecycle::claim_pid_file(&cfg.pid_file(), std::process::id()) {
        eprintln!(
            "doorbell serve: pidfile claim failed ({}): {e}; continuing unclaimed",
            cfg.pid_file().display()
        );
    }
    let started = std::time::Instant::now();
    let log_path = cfg.log_file();
    // Precondition, named loudly: the daemon execs `toron` (bus watch +
    // inbox fetch) and `herdr` (prompt sink). Under launchd/systemd neither
    // is on PATH; resolving them here to a real absolute path is what keeps
    // the ring alive, and the miss is stated once instead of being inferred
    // from a wake that never arrives.
    let bins = resolved_bins();
    eprintln!(
        "doorbell serve: cli toron={} (resolved={}) herdr={} (resolved={})",
        bins.toron, bins.toron_resolved, bins.herdr, bins.herdr_resolved
    );
    if !bins.toron_resolved || !bins.herdr_resolved {
        eprintln!(
            "doorbell serve: WARNING no `toron`/`herdr` on PATH, beside this binary, or in \
             ~/.cargo/bin, ~/.local/bin, ~/.local/share/mise/shims — wakes will NOT be \
             delivered; set DOORBELL_TORON_BIN / DOORBELL_HERDR_BIN or install the binaries"
        );
    }
    // bd-2o1v: `hosted` starts EMPTY — `spawn_host_loop` is the sole writer,
    // so the list names only loops that actually exist (a pre-populated
    // list made the idempotency guard skip every boot host: health showed
    // hosted strings with no Brain behind them, zero sweeps, clean exit).
    let hosted: Hosts = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    eprintln!(
        "doorbell serve: up on 127.0.0.1:{} + {} (hosting {})",
        cfg.port,
        sock_path.display(),
        if cfg.hosts.is_empty() {
            "nothing (standby)".to_string()
        } else {
            cfg.hosts
                .iter()
                .map(|(p, w)| format!("{p}/{w}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    // The cache is created before workers so a worker supervisor can
    // invalidate /health in the same task turn that removes a dead host.
    let health: Shared<CachedHealth> = std::sync::Arc::new(parking_lot::Mutex::new(CachedHealth {
        body: String::new(),
        computed_at: std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or(std::time::Instant::now()),
    }));
    // Host loops: one Brain per configured workspace. Supervisors own the
    // JoinHandles, remove dead workers from `hosted`, release their lease,
    // and invalidate the health snapshot before the next probe can advertise
    // a worker that no longer exists.
    let loops: Loops = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    for (project, workspace) in &cfg.hosts {
        spawn_host_loop(cfg, &hosted, &loops, &owned, &health, project, workspace);
    }
    // bd-lorot: the workspaces this daemon actually hosts, captured BEFORE the
    // tick moves `cfg`. The prune may only touch workspaces outside this set.
    let prune_live: std::collections::BTreeSet<String> =
        cfg.hosts.iter().map(|(_, ws)| ws.clone()).collect();
    let prune_state = cfg.flywheel_state.clone();
    let rotate_tick = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            copy_truncate_over(&log_path, LOGROTATE_BYTES);
            // bd-lorot: journals are one file per workspace the plane has EVER
            // served and nothing removed them, so known_workspaces only grew —
            // 913 files / 20 MB measured, and it is the input to the standby
            // wake-blind signal. Prune the ones this daemon does not host AND
            // has not touched for a week; a just-spawned crew is never a
            // candidate, so a live workspace's journal cannot be deleted.
            let store = doorbell_rt::FileStore::new(prune_state.clone());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let report = store.prune_unhosted_workspace_logs(
                &prune_live,
                UNHOSTED_JOURNAL_MAX_AGE_SECS,
                now,
            );
            if report.journals_removed > 0 {
                eprintln!(
                    "doorbell: pruned {} journal file(s) for unhosted workspaces, freed {} byte(s) ({} left: too young)",
                    report.journals_removed, report.bytes_freed, report.skipped_young
                );
            }
        }
    });
    let app = AppState {
        started,
        fw_state: cfg.flywheel_state.clone(),
        hosted: hosted.clone(),
        health: health.clone(),
    };
    // bd-o86s: the /health snapshot tick. Recomputes the body on its own
    // cadence so request handlers never pay the shells-and-greps cost.
    // Initial compute happens inline once (cold start), then every TTL.
    {
        let app = app.clone();
        refresh_health_snapshot(&app);
    }
    let health_tick = {
        let app = app.clone();
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(HEALTH_CACHE_TTL_SECS));
            loop {
                tick.tick().await;
                // Compute off the request path; a slow compute only delays
                // the NEXT snapshot, never a served request.
                let app = app.clone();
                tokio::task::spawn_blocking(move || refresh_health_snapshot(&app))
                    .await
                    .ok();
            }
        })
    };
    // Detached supervision: the tick aborts with the serve task group; the
    // handle is dropped (not awaited) on purpose — awaiting it here would
    // block the accept loop.
    drop(health_tick);
    let serve_tcp = async {
        loop {
            let (stream, _) = tcp.accept().await?;
            let app = app.clone();
            tokio::spawn(handle_http(stream, app));
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let serve_uds = async {
        loop {
            let (stream, _) = uds.accept().await?;
            let app = app.clone();
            tokio::spawn(handle_uds(stream, app));
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    // D9 layer 1 upkeep: heartbeat every 30s per held lease. Dynamic hosts
    // (bd-2o1v auto-host) add leases to the same shared set, so they are
    // heartbeated identically to boot-time hosts.
    let hb_state = cfg.flywheel_state.clone();
    let hb_owned = owned.clone();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            let now = doorbell_rt::lease::unix_now();
            for (ws, _) in hb_owned.lock().iter() {
                if let Some(mut cur) = doorbell_rt::lease::read_lease(&hb_state, ws) {
                    doorbell_rt::lease::refresh_lease(&hb_state, ws, &mut cur, now);
                }
            }
        }
    });
    // bd-2o1v reconcile: subscriptions are the hosting truth. Every 60s the
    // daemon hosts every subscribed (project, workspace) it can name
    // (lease-fenced — never steal a live writer) and reaps spawn-scoped
    // rows whose workspace went silent for a week.
    let reconcile = std::sync::Arc::new(tokio::spawn({
        let cfg = cfg.clone();
        let hosted = hosted.clone();
        let loops = loops.clone();
        let owned = owned.clone();
        let health = health.clone();
        async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                reconcile_once(&cfg, &hosted, &loops, &owned, &health);
            }
        }
    }));
    let finish = || {
        heartbeat.abort();
        reconcile.abort();
        rotate_tick.abort();
        crate::lifecycle::release_pid_file(&cfg.pid_file(), std::process::id());
        for (ws, l) in owned.lock().iter() {
            doorbell_rt::lease::release_lease(&cfg.flywheel_state, ws, l);
        }
    };
    // Graceful shutdown on Ctrl-C and SIGTERM (`doorbell stop` sends
    // TERM: drain sinks, journal undelivered intents, release leases).
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM listener");
    #[cfg(unix)]
    let shutdown = async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    };
    #[cfg(not(unix))]
    let shutdown = async {
        tokio::signal::ctrl_c().await.ok();
    };
    let accepting = tokio::select! {
        r = serve_tcp => Some(r),
        r = serve_uds => Some(r),
        _ = shutdown => None,
    };
    // Free the endpoints FIRST on SIGTERM: `tcp`/`uds` are owned here, so
    // dropping the accept futures above does NOT close the listeners — a
    // listener whose accept loop is gone still accepts into the backlog
    // without ever answering, every probe then times out, and the dying
    // daemon is misread as a "non-doorbell squatter" (the restart-storm
    // root cause). Shutdown is therefore: listeners closed, drain bounded,
    // leases released, exit.
    drop(tcp);
    drop(uds);
    match accepting {
        Some(r) => {
            finish();
            r
        }
        None => {
            // Shutdown travels as a message (ticker + forwarder senders
            // stay open until their tasks end with the brain).
            eprintln!("doorbell serve: shutdown signal received, draining loops");
            let retiring = std::mem::take(&mut *hosted.lock());
            for (key, why) in send_shutdowns(&retiring, SEND_BUDGET).await {
                let hint = match why {
                    SendFail::ChannelClosed => "stale hosted row".to_string(),
                    SendFail::BudgetExhausted => format!(
                        "no Shutdown enqueued within {SEND_BUDGET:?} — the brain dies with the process"
                    ),
                };
                eprintln!("doorbell serve: shutdown send FAILED ({why}) for {key} — {hint}");
            }
            eprintln!("doorbell serve: shutdown sent, joining loops");
            drop(retiring);
            let mut entries = std::mem::take(&mut *loops.lock());
            // Bounded drain: SIGTERM must always finish inside stop's 20s
            // grace (drain 10s + probe window + exit allowance, bd-pf3b). Joins race the budget CONCURRENTLY (bd-zzgb): the old
            // head-of-line await meant one hung Brain stalled the scan and
            // the note claimed every loop missed the deadline even when the
            // ones behind it had already exited. At the deadline the stragglers
            // are named loudly, then the process exits (their handles drop here;
            // the tasks detach — bd-nuf2: intents were journaled pre-delivery,
            // resume redelivers).
            let report = drain_loops(&mut entries, DRAIN_BUDGET).await;
            eprintln!(
                "{}",
                drain_note(report.joined, report.remaining, report.total)
            );
            if let Some(note) = straggler_note(&report) {
                eprintln!("{note}");
            }
            drop(entries);
            finish();
            eprintln!("doorbell serve: shutting down");
            Ok(())
        }
    }
}

/// Pure boot-gate predicate (bd-fuql): hosting needs BOTH CLIs resolvable.
/// Tested without spawning anything; `spawn_host_loop` enforces it.
fn bins_allow_hosting(toron_resolved: bool, herdr_resolved: bool) -> bool {
    toron_resolved && herdr_resolved
}

/// Spawn one hosted loop (bd-2o1v: shared by boot-time `--host` config and
/// dynamic auto-host) and record it on the shared tables.
fn spawn_host_loop(
    cfg: &Config,
    hosted: &Hosts,
    loops: &Loops,
    owned: &Shared<Vec<(String, doorbell_rt::lease::OwnerLease)>>,
    health: &Shared<CachedHealth>,
    project: &str,
    workspace: &str,
) {
    // Idempotent: two callers (boot config + reconcile auto-host) may race
    // the same pair — a second Brain on one workspace double-rings. Check
    // before pushing anything, so an early return leaves no orphan tx.
    let key = format!("{project}/{workspace}");
    if hosted.lock().iter().any(|h| h.key == key) {
        return;
    }
    // bd-fuql: fail-closed boot gate — hosting with an unresolvable toron
    // or herdr CLI is a deaf plane (/health would merely decorate it with
    // cli_unresolved while no wake can land). Refuse here: both boot config
    // and reconcile auto-host funnel through this fn. The operator pins
    // real bins via DOORBELL_TORON_BIN / DOORBELL_HERDR_BIN.
    let early_bins = resolved_bins();
    if !bins_allow_hosting(early_bins.toron_resolved, early_bins.herdr_resolved) {
        let mut missing = Vec::new();
        if !early_bins.toron_resolved {
            missing.push("toron");
        }
        if !early_bins.herdr_resolved {
            missing.push("herdr");
        }
        eprintln!(
            "doorbell serve: REFUSING to host {key} — unresolvable CLI bins ({}) — set {} to absolute binary paths",
            missing.join(", "),
            missing
                .iter()
                .map(|b| if *b == "toron" {
                    doorbell_rt::binpath::TORON_BIN_ENV
                } else {
                    doorbell_rt::binpath::HERDR_BIN_ENV
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        release_owned_lease(&cfg.flywheel_state, owned, workspace);
        return;
    }
    // bd-ba8t: say out loud whether this workspace's wakes have anywhere to
    // land. Silence here is exactly how a hosted-but-deaf plane looks healthy.
    // Two `herdr agent list` shells per host is acceptable at boot; /health
    // recomputes it live for readers.
    let inj = doorbell_rt::file_host::injection_picture(
        &cfg.flywheel_state,
        project,
        None,
        &resolved_bins().herdr,
    );
    eprintln!(
        "doorbell serve: hosting {key} — {} pin(s), {} pane(s) in herdr, {} injectable now",
        inj.pins, inj.panes_seen, inj.injectable
    );
    if inj.pins > 0 && inj.panes_seen == 0 {
        eprintln!(
            "doorbell serve: WARNING hosting {key} — {} pin(s) but herdr knows none of their \
             panes: NO wake can be injected here (inbox wakes queue/digest instead). Start agents \
             (`herdr agent start`) or fix the stale registry",
            inj.pins
        );
    }
    let (tx, rx) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
    let (fw, p, w) = (
        cfg.flywheel_state.clone(),
        project.to_string(),
        workspace.to_string(),
    );
    let loop_tx = tx.clone();
    let supervised_key = key.clone();
    let supervised_workspace = workspace.to_string();
    let supervised_state = cfg.flywheel_state.clone();
    let supervised_hosted = hosted.clone();
    let supervised_owned = owned.clone();
    let supervised_health = health.clone();
    // bd-y40k: ONE probe per loop, shared by the brain's touches (inside
    // the task) and the drain's deadline read (from the table entry).
    let liveness = std::sync::Arc::new(doorbell_rt::brain::BrainLiveness::new());
    let supervised_liveness = liveness.clone();
    // Register before spawning: a very fast startup failure must not win a
    // race against registration and leave a stale hosted row behind.
    hosted.lock().push(HostedLoop {
        key: key.clone(),
        tx: tx.clone(),
    });
    let task = tokio::spawn(async move {
        // bd-lmtu: START THE HEARTBEAT. `spawn_heartbeat_ticker` had NO caller
        // anywhere outside its own module, so the 1s ticker was dead code: `.hb`
        // was only ever touched incidentally by the wake-outcome arm in
        // file_host, which is why the live state files were 596s and 57540s old
        // rather than ~1s, and why `/health` reported
        // `wake.pipe.class: "NeverStamped"` with `last_heartbeat_ms: null` for
        // every workspace.
        //
        // That matters because the heartbeat is what lets wake-liveness tell a
        // QUIET host from a DEAF one. With no stamp there is nothing to classify
        // against, so liveness degrades to NeverStamped and a wake cannot be
        // shown to have landed.
        //
        // Lifespan is the loop's: `_hb` is a Sender whose Drop ends the ticker
        // thread, and an async block's locals are dropped on normal return AND
        // on abort — so the ticker cannot outlive the hosted loop, and a retired
        // loop (bd-mk60) stops stamping.
        let _hb =
            doorbell_rt::watchbeat::spawn_heartbeat_ticker(supervised_state.clone(), w.clone());
        // Await a child task so panic/abort is also a supervised completion:
        // the outer task still removes the hosted row and releases its lease.
        let child = tokio::spawn(host_workspace(fw, p, w, loop_tx, rx, supervised_liveness));
        let report = match child.await {
            Ok(report) => report,
            Err(error) => {
                eprintln!("doorbell serve: hosted loop {supervised_key} failed: {error}");
                doorbell_rt::RunReport {
                    undelivered: Vec::new(),
                    stats: doorbell_rt::api::Stats::default(),
                }
            }
        };
        eprintln!(
            "doorbell serve: supervised loop {supervised_key} exited (injects={} sheds={} degraded={})",
            report.stats.injects, report.stats.sheds, report.stats.degraded
        );
        if remove_host(&supervised_hosted, &supervised_key) {
            release_owned_lease(&supervised_state, &supervised_owned, &supervised_workspace);
            invalidate_health_snapshot(&supervised_health);
            eprintln!("doorbell serve: removed dead hosted worker {supervised_key} from health");
        }
    });
    loops.lock().push(LoopEntry {
        key,
        task,
        liveness: Some(liveness),
    });
    // The table keeps its OWN sender: the loop's clones live inside the task,
    // so this handle is the only one reconcile can address. `try_send` on it
    // is how a retiring loop is told to drain (bd-mk60).
}
/// auto-host of every nameable subscribed (project, workspace).
fn reconcile_once(
    cfg: &Config,
    hosted: &Hosts,
    loops: &Loops,
    owned: &Shared<Vec<(String, doorbell_rt::lease::OwnerLease)>>,
    health: &Shared<CachedHealth>,
) {
    loops.lock().retain(|entry| !entry.task.is_finished());
    use doorbell_rt::lease;
    use doorbell_rt::subscribe::{Actor, SubscriptionStore};
    let mut store = SubscriptionStore::open(&cfg.flywheel_state);
    for id in reap_candidates(&store.list(), |ws| {
        journal_age_secs(&cfg.flywheel_state, ws)
    }) {
        match store.unsubscribe(Actor::operator(), &id) {
            Ok(dropped) => {
                eprintln!(
                    "doorbell serve: reaped stale subscription {} (journal silent > {}s)",
                    dropped.id, REAP_AFTER_SECS
                );
            }
            Err(e) => eprintln!("doorbell serve: reap {id} failed: {e:?}"),
        }
    }
    let now = lease::unix_now();
    // Bind the work list BEFORE looping: the `hosted` guard must not be
    // held across the loop body — `spawn_host_loop` locks it again and
    // parking_lot mutexes are not reentrant (self-deadlock wedged the
    // whole runtime: every handler parked in lock_slow behind /health).
    let subs = store.list();
    // Bind the key snapshot to a LOCAL: `&hosted_keys(&hosted.lock())` as a
    // call argument keeps the guard alive until the end of the call
    // statement, and `retire_unsubscribed` locks `hosted` again — parking_lot
    // mutexes are not reentrant, so the inline form self-deadlocks the
    // reconcile task forever (and every /health handler behind it).
    let unhosted = unhosted_subscribed(&subs, &hosted_keys(&hosted.lock()));
    for (project, workspace) in unhosted {
        if let Some(cur) = lease::read_lease(&cfg.flywheel_state, &workspace)
            && !lease::lease_stale(&cur, now)
        {
            continue; // a live writer (embedded loop) owns it — never steal
        }
        match lease::acquire_lease(&cfg.flywheel_state, &workspace, now) {
            Ok(l) => {
                owned.lock().push((workspace.clone(), l));
                eprintln!("doorbell serve: auto-hosting {project}/{workspace} (subscribed)");
                // The worker shares the served cache, so an immediate
                // startup failure invalidates the same snapshot /health uses.
                spawn_host_loop(cfg, hosted, loops, owned, health, &project, &workspace);
            }
            Err(e) => {
                eprintln!("doorbell serve: auto-host {project}/{workspace} lease failed: {e:?}")
            }
        }
    }
    // bd-mk60: retire what this tick did not justify. Runs AFTER the auto-host
    // pass so a pair that is still subscribed is already hosted — and that
    // ordering is what makes the retire rule safe: an auto-hosted key is in
    // `subscribed_keys(subs)`, so it can never be retired on the same tick.
    let hosted_now = hosted_keys(&hosted.lock());
    retire_unsubscribed(cfg, hosted, owned, health, &subs, &hosted_now);
}

/// bd-mk60: stop the loops whose subscriptions went away.
///
/// Reconcile could add hosts but never remove them: unsubscribing a
/// workspace left its Brain sweeping forever and holding a writer lease.
///
/// Retirement is a `Shutdown` message, never a bare task abort — the brain
/// drains its sinks, journals its undelivered intents and returns a report,
/// which an abort would throw away. The writer lease is released here for the
/// same reason `finish()` releases the others: a retired loop no longer
/// writes, so holding it would block a peer's embedded loop.
fn retire_unsubscribed(
    cfg: &Config,
    hosted: &Hosts,
    owned: &Shared<Vec<(String, doorbell_rt::lease::OwnerLease)>>,
    health: &Shared<CachedHealth>,
    subs: &[doorbell_rt::subscribe::Subscription],
    hosted_now: &[String],
) {
    let boot: Vec<String> = cfg.hosts.iter().map(|(p, w)| format!("{p}/{w}")).collect();
    let retiring = retire_keys(hosted_now, &boot, &subscribed_keys(subs));
    if retiring.is_empty() {
        return;
    }
    let mut stopped: Vec<HostedLoop> = Vec::new();
    {
        let mut table = hosted.lock();
        table.retain(|l| {
            if retiring.contains(&l.key) {
                stopped.push(HostedLoop {
                    key: l.key.clone(),
                    tx: l.tx.clone(),
                });
                false
            } else {
                true
            }
        });
    }
    if !stopped.is_empty() {
        invalidate_health_snapshot(health);
    }
    for l in &stopped {
        eprintln!(
            "doorbell serve: retiring {} (no subscription references it)",
            l.key
        );
        // Not `try_send`: a full channel must not silently keep the loop
        // alive. Spawn the send so reconcile never blocks on a busy brain.
        let tx = l.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(doorbell_rt::BrainMsg::Shutdown).await;
        });
        let Some(ws) = hosted_workspace(&l.key).map(str::to_string) else {
            continue;
        };
        let mut o = owned.lock();
        if let Some(pos) = o.iter().position(|(w, _)| w == &ws) {
            let (_, lease) = o.remove(pos);
            doorbell_rt::lease::release_lease(&cfg.flywheel_state, &ws, &lease);
        }
    }
}

fn remove_host(hosted: &Hosts, key: &str) -> bool {
    let mut hosts = hosted.lock();
    let before = hosts.len();
    hosts.retain(|host| host.key != key);
    hosts.len() != before
}

fn release_owned_lease(
    state: &std::path::Path,
    owned: &Shared<Vec<(String, doorbell_rt::lease::OwnerLease)>>,
    workspace: &str,
) {
    let mut leases = owned.lock();
    if let Some(pos) = leases.iter().position(|(ws, _)| ws == workspace) {
        let (_, lease) = leases.remove(pos);
        doorbell_rt::lease::release_lease(state, workspace, &lease);
    }
}

fn invalidate_health_snapshot(health: &Shared<CachedHealth>) {
    health.lock().computed_at = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(HEALTH_CACHE_TTL_SECS + 1))
        .unwrap_or_else(std::time::Instant::now);
}

fn parent_of(p: &std::path::Path) -> &std::path::Path {
    p.parent().unwrap_or(std::path::Path::new("/tmp"))
}

/// Who holds a port we could not bind, as one /health probe tells it:
/// a healthy doorbell peer, an unresponsive holder (the socket accepts
/// but nothing answers — a daemon mid-shutdown or wedged), or a foreign
/// process answering with something else entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortHolder {
    Peer,
    Unresponsive,
    Foreign,
}

fn classify_port_holder(health: Option<&serde_json::Value>) -> PortHolder {
    match health {
        None => PortHolder::Unresponsive,
        Some(v)
            if v.get("protocol").and_then(|p| p.as_str())
                == Some(doorbell_rt::api::PING.protocol) =>
        {
            PortHolder::Peer
        }
        Some(_) => PortHolder::Foreign,
    }
}

/// Minimal blocking /health probe for the bind fence (async callers use
/// this before deciding exit-0 vs hard-fail).
async fn peer_health(cfg: &Config) -> Option<serde_json::Value> {
    let body = tokio::task::spawn_blocking({
        let port = cfg.port;
        move || {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().ok()?,
                std::time::Duration::from_secs(2),
            )
            .ok()?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .ok()?;
            write!(
                stream,
                "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            )
            .ok()?;
            let mut body = String::new();
            stream.read_to_string(&mut body).ok()?;
            let idx = body.find("\r\n\r\n")?;
            Some(body[idx + 4..].to_string())
        }
    })
    .await
    .ok()??;
    serde_json::from_str(&body).ok()
}

/// Hourly 1 MiB copy-truncate (toron parity shape): readers keep their
/// offset, the file never grows without bound (the 1s-watch RSS lesson).
fn copy_truncate_over(path: &std::path::Path, cap: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= cap {
        return;
    }
    if std::fs::copy(path, path.with_extension("log.1")).is_ok() {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path);
    }
}

/// Recompute the served snapshot now (bd-o86s): the ONLY place
/// `health_body` runs in the daemon — the tick and cold start call this,
/// request handlers never do.
fn refresh_health_snapshot(app: &AppState) {
    let body = health_body(app);
    *app.health.lock() = CachedHealth {
        body,
        computed_at: std::time::Instant::now(),
    };
}

/// Serve /health from the snapshot (bd-o86s): a fresh cache returns
/// instantly (no shells, no greps); a stale/cold cache recomputes inline
/// once so the response is never older than TTL plus one compute.
fn cached_health_body(app: &AppState) -> String {
    let fresh = app.health.lock().computed_at.elapsed().as_secs() < HEALTH_CACHE_TTL_SECS;
    let body = app.health.lock().body.clone();
    if fresh && !body.is_empty() {
        return body;
    }
    refresh_health_snapshot(app);
    app.health.lock().body.clone()
}

async fn handle_http(mut stream: tokio::net::TcpStream, app: AppState) {
    let mut buf = vec![0u8; 4096];
    let Ok(n) = stream.read(&mut buf).await else {
        return;
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    let (code, content_type, body) = match path.split('?').next().unwrap_or("/") {
        "/health" => ("200 OK", "application/json", cached_health_body(&app)),
        "/stats" => ("200 OK", "application/json", stats_body(&app)),
        "/events" => {
            let since = path
                .split("since=")
                .nth(1)
                .and_then(|s| s.split('&').next()?.parse::<u64>().ok())
                .or_else(|| {
                    req.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("last-event-id")
                            .then(|| v.trim().parse::<u64>().ok())?
                    })
                });
            (
                "200 OK",
                "text/event-stream",
                events_body(&app.fw_state, since),
            )
        }
        _ => (
            "404 Not Found",
            "application/json",
            serde_json::json!({"error": "not found"}).to_string(),
        ),
    };
    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 {code}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
}

async fn handle_uds(mut stream: tokio::net::UnixStream, app: AppState) {
    // UDS speaks the same minimal surface: one JSON line in, one out.
    // Verbs ride here in Phase 2; today only `ping` answers.
    let mut buf = vec![0u8; 4096];
    let Ok(n) = stream.read(&mut buf).await else {
        return;
    };
    let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    let body = if req.contains("ping") {
        let p = &doorbell_rt::api::PING;
        serde_json::json!({"protocol": p.protocol, "core_v": p.core_v, "journal_v": p.journal_v})
            .to_string()
            + "\n"
    } else {
        serde_json::json!({"alive": true, "uptime_s": app.started.elapsed().as_secs()}).to_string()
            + "\n"
    };
    let _ = stream.write_all(body.as_bytes()).await;
}

/// Journal mtime age per hosted workspace — sweep liveness without
/// shared mutable state (the journal IS the ground truth D4 points to).
/// Workspace half of a hosted `project/workspace` pair. `rsplit_once('/')`
/// matches `parse_host`: project keys are routinely absolute paths, so the
/// FIRST `/` splits at `/private` and every downstream lookup (sweep age,
/// journal seq, watch heartbeat, subscriber scope) silently reads the wrong
/// workspace — `/health` then reported nulls and zeros for a live daemon.
fn hosted_workspace(hw: &str) -> Option<&str> {
    hw.rsplit_once('/')
        .map(|(_, ws)| ws)
        .filter(|ws| !ws.is_empty())
}

/// bd-ba8t: the prompt hop's precondition, as counts. A registry pin whose
/// pane `herdr agent list` does not know can never be injected into, no
/// matter how well its wake classifies. Summed per hosted PROJECT (the
/// registry file is project-scoped), not per workspace, so two workspaces of
/// one project are not double-counted.
fn hosted_injection(
    fw_state: &std::path::Path,
    hosted: &[String],
    herdr_bin: &str,
) -> doorbell_rt::file_host::InjectionPicture {
    use doorbell_rt::file_host::{InjectionPicture, injection_picture};
    let mut total = InjectionPicture::default();
    let mut projects: Vec<&str> = Vec::new();
    for h in hosted {
        let Some((project, _)) = h.split_once('/') else {
            continue;
        };
        if projects.contains(&project) {
            continue;
        }
        projects.push(project);
        let pic = injection_picture(fw_state, project, None, herdr_bin);
        total.pins += pic.pins;
        total.panes_seen += pic.panes_seen;
        total.injectable += pic.injectable;
        total.panes_missing += pic.panes_missing;
    }
    total
}

/// bd-ba8t: when does the ring's LAST hop become structurally impossible?
/// Only when the daemon hosts something AND the registry names pins AND
/// herdr knows the panes of NONE of them. A fleet caught mid-work (every
/// pane `working`) is healthy: the durable queue holds the wake and drains
/// on the next `idle` transition.
fn prompt_hop_degraded(
    hosted_pairs: usize,
    pic: &doorbell_rt::file_host::InjectionPicture,
) -> bool {
    hosted_pairs > 0 && pic.pins > 0 && pic.panes_seen == 0
}

/// Wake subscriptions registered for the daemon's hosted workspaces
/// (flywheel `spawn` writes one per pin as `Scope::One(workspace)`). A
/// standby daemon hosts nothing, so 0 is the honest answer there — but it
/// is now counted rather than asserted.
fn hosted_subscribers(fw_state: &std::path::Path, hosted: &[String]) -> usize {
    let workspaces: Vec<&str> = hosted.iter().filter_map(|h| hosted_workspace(h)).collect();
    if workspaces.is_empty() {
        return 0;
    }
    doorbell_rt::SubscriptionStore::open(fw_state)
        .list()
        .iter()
        .filter(|s| {
            matches!(&s.scope, doorbell_rt::Scope::One(ws) if workspaces.contains(&ws.as_str()))
        })
        .count()
}

/// Age of the most recent bus event across the hosted workspaces (min age =
/// most recent), from the `.event` stamp only.
///
/// GS-062: `watch.last_event_age_s` is **event-only** — under Quiet wake
/// (heartbeats alive, no lines) this is `null`/stale while `wake.class=Quiet`.
fn hosted_watch_age(fw_state: &std::path::Path, hosted: &[String]) -> Option<u64> {
    // GS-062: last_event_age_s from the **event** stamp (not hb).
    hosted
        .iter()
        .filter_map(|h| hosted_workspace(h))
        .filter_map(|ws| doorbell_rt::watchbeat::event_age_secs(fw_state, ws))
        .min()
}

/// GS-062 ASK-USER E + F (captain locked): wake_liveness Quiet/Live/Deaf on
/// operator `/health` (:18980), using hosted AppState workspace keys
/// (same labels stamp writers use). Ask-user F: **worst-of** across ALL
/// hosted workspaces (consistent with `hosted_watch_age` min-across-hosted),
/// not primary-only. In-memory HostedLoops are all alive — empty hosted →
/// NeverStamped.
fn hosted_wake_pipe(fw_state: &std::path::Path, hosted: &[String]) -> serde_json::Value {
    use doorbell_rt::wake_liveness::{self, DEFAULT_DEAF_CEILING_MS};
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let workspaces: Vec<&str> = hosted.iter().filter_map(|h| hosted_workspace(h)).collect();
    // Worst-of over every hosted workspace (ask-user F). Never max-merge
    // stamps then classify once (Live would mask Deaf). Winner label becomes
    // wake.workspace so stamps match the class (not first-alive).
    let stamps: Vec<(&str, Option<u64>, Option<u64>)> = workspaces
        .iter()
        .map(|ws| {
            (
                *ws,
                doorbell_rt::watchbeat::last_event_ms(fw_state, ws),
                doorbell_rt::watchbeat::last_heartbeat_ms(fw_state, ws),
            )
        })
        .collect();
    let (pipe, worst_ws, last_event_ms, last_heartbeat_ms) =
        wake_liveness::classify_worst_of_stamps(now_ms, stamps, DEFAULT_DEAF_CEILING_MS);
    let workspace = worst_ws.unwrap_or("");
    serde_json::json!({
        "workspace": workspace,
        "hosted": workspaces,
        "pipe": wake_liveness::to_json(pipe),
        "last_event_ms": last_event_ms,
        "last_heartbeat_ms": last_heartbeat_ms,
        "deaf_ceiling_ms": DEFAULT_DEAF_CEILING_MS,
    })
}

fn hosted_sweep_age(fw_state: &std::path::Path, hosted: &[String]) -> (Option<u64>, u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut min_age: Option<u64> = None;
    let mut max_seq = 0u64;
    for hw in hosted {
        let Some(ws) = hosted_workspace(hw) else {
            continue;
        };
        let path = fw_state
            .join("journals")
            .join(format!("doorbell-journal-{ws}.jsonl"));
        if let Ok(md) = std::fs::metadata(&path) {
            let age = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| now.saturating_sub(t.as_secs()));
            min_age = Some(match (min_age, age) {
                (Some(a), Some(b)) => a.min(b),
                (a, b) => a.or(b).unwrap_or(u64::MAX),
            });
        }
        let store = doorbell_rt::FileStore::new(fw_state);
        max_seq = max_seq.max(store.last_seq(ws));
    }
    (min_age.filter(|a| *a != u64::MAX), max_seq)
}
/// The build identity of the RUNNING executable. The IO half of bd-k0p7: it
/// resolves `current_exe` and stats it, then hands the observation to the
/// pure [`doorbell_rt::api::resolve_build_stamp`]. Never trusts the source
/// tree or a compile-time constant for the age — the running binary is the
/// artifact under test.
pub(super) fn running_build_stamp(now_unix: u64) -> doorbell_rt::api::BuildStamp {
    let Ok(exe) = std::env::current_exe() else {
        // Cannot name the exe: the compiled-in version still stands, and the
        // observed fields stay absent rather than reporting a fake zero.
        return doorbell_rt::api::resolve_build_stamp(None, now_unix);
    };
    let observed = std::fs::metadata(&exe).ok().and_then(|md| {
        let mtime = md
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some((mtime, md.len()))
    });
    let mut stamp = doorbell_rt::api::resolve_build_stamp(observed, now_unix);
    // Set the path AFTER resolving: the resolver owns the observed fields, and
    // reading `current_exe` is the only step that can fail.
    stamp.exe = Some(exe.display().to_string());
    stamp
}

/// Is the running build actually BEHIND a source change?
///
/// bd-k0p7 built this field to catch a real incident: a committed doorbell-cli
/// fix sat unrun for 11h because flywheel was rebuilt and doorbell was not. But
/// it implemented the catch as a WALL-CLOCK AGE test (`age_s` older than a
/// fixed six-hour window),
/// and age is a proxy for staleness, not staleness. Measured 2026-09-28 on this
/// host: the only doorbell source change in the preceding 4.5h was a
/// rustfmt-only commit (3 insertions, 12 deletions, all line rewrapping), so the
/// running daemon was byte-for-byte equivalent to the tree — and reported
/// `degraded: [stale_build]`, because it had been up longer than the window.
///
/// A field that fires on a healthy daemon is worse than no field: an operator
/// learns to skip it, and then it is also skipped when it is true. So the
/// question is the real one: is there SOURCE newer than the running exe?
///
/// The 11h incident is still caught, because its source genuinely was newer.
/// `age_s` stays published as ADVISORY, since uptime is real context.
///
/// Unknown means unknown, never stale — a stamp with no exe mtime cannot be
/// behind anything, and a deployed binary with no source tree present cannot be
/// compared, so it must not cry wolf.
pub(super) fn is_build_behind_source(
    exe_mtime: Option<u64>,
    newest_source_mtime: Option<u64>,
) -> bool {
    match (exe_mtime, newest_source_mtime) {
        (Some(exe), Some(src)) => src > exe,
        // No exe stamp, or no source to compare against: unanswerable, and an
        // unanswerable check must not read as a fault.
        _ => false,
    }
}

/// Newest mtime under the doorbell source crates, or `None` when the tree is
/// not where this binary was built (a deployed copy). Best-effort by design:
/// its only job is to make `is_build_behind_source` answerable, and a miss is
/// the safe direction.
fn newest_doorbell_source_mtime() -> Option<u64> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)?;
    let mut newest: Option<u64> = None;
    // A work-list rather than a recursive closure: recursion needs a named fn,
    // and this stays a plain loop with no helper to unit test separately.
    let mut stack: Vec<std::path::PathBuf> = ["doorbell-cli", "doorbell-rt"]
        .iter()
        .map(|d| root.join("crates").join(d).join("src"))
        .collect();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let Ok(m) = e.metadata().and_then(|m| m.modified()) else {
                    continue;
                };
                if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                    let secs = d.as_secs();
                    newest = Some(newest.map_or(secs, |n: u64| n.max(secs)));
                }
            }
        }
    }
    newest
}

/// bd-enov (F17): workspaces the plane has EVER served — one journal file per
/// workspace, written by the hosted loop. Their presence while `mode=standby`
/// is the wake-blind signature: a fleet exists that expects rings/digests, and
/// this daemon delivers none.
fn known_journal_workspaces(fw_state: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(fw_state.join("journals")) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name
                .strip_prefix("doorbell-journal-")
                .and_then(|n| n.strip_suffix(".jsonl"))
                && !rest.is_empty()
                && !rest.contains('.')
            {
                out.push(rest.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A daemon in standby that already knows workspaces is blind to every wake.
///
/// Standby means no hosted loop, so no rings, no inbox-digest wake and no
/// ack-SLA nudge are delivered, and nothing in the daemon notices. This is also
/// the exact shape `/health` reports for the first seconds after a restart,
/// before the daemon auto-hosts from subscriptions: a boot window, not a failed
/// restart. A standby daemon over an empty state dir is legitimately quiet and
/// stays green.
pub(super) fn standby_blind_to_wakes(hosted: usize, known_workspaces: usize) -> bool {
    hosted == 0 && known_workspaces > 0
}

fn health_body(app: &AppState) -> String {
    // bd-2o1v: hosting is dynamic — snapshot under the lock once.
    // GS-062: alive-first + alphabetical slug (not insertion order).
    let hosted = hosted_keys_for_health(&app.hosted.lock());
    // D4.4: a torn journal tail degrades loudly here (bd-asr2.4).
    let store = doorbell_rt::FileStore::new(&app.fw_state);
    let mut degraded = Vec::new();
    // bd-lorot: scoped to what we HOST. Scanning every journal let an abandoned
    // workspace hold `degraded` red forever.
    let hosted_ws: Vec<&str> = hosted.iter().filter_map(|h| hosted_workspace(h)).collect();
    // One fold, not two: the detailed probe answers the boolean AND the numbers.
    let tears = store.torn_tails_among(&hosted_ws);
    if !tears.is_empty() {
        degraded.push("journal_torn_tail".to_string());
    }
    let mode = if hosted.is_empty() {
        "standby"
    } else {
        "hosted"
    };
    // bd-enov (F17): standby is NOT green when known workspaces exist. A daemon
    // that adopted without `--host` (plain `doorbell start`) reports
    // mode=standby and delivers no rings, no inbox-digest wakes, no ack-SLA
    // nudges — every wake path degrades to manual and NOTHING notices. Mark it
    // degraded and name the arming verb so `arrive`/`briefing`/`status` see it.
    let known_workspaces = known_journal_workspaces(&app.fw_state);
    let wake_blind = standby_blind_to_wakes(hosted.len(), known_workspaces.len());
    if wake_blind {
        degraded.push("standby_with_known_workspaces".to_string());
    }
    let (sweep_age, max_seq) = hosted_sweep_age(&app.fw_state, &hosted);
    let watch_age = hosted_watch_age(&app.fw_state, &hosted);
    // ASK-USER E: Quiet/Live/Deaf via wake_liveness on hosted AppState keys.
    let wake = hosted_wake_pipe(&app.fw_state, &hosted);
    // D2/P3: the daemon's own edges degrade loudly. Two facts that were
    // live-invisible: every inbox fetch failing (`inbox_error` audits) and a
    // watch child that dies far faster than its idle-exit cadence
    // (`RestartWatch` rows). Both are read back from the ground-truth
    // surfaces rather than tracked in RAM, so they survive a restart.
    let bins = resolved_bins();
    if !bins.toron_resolved || !bins.herdr_resolved {
        degraded.push("cli_unresolved".to_string());
    }
    let now = doorbell_rt::lease::unix_now();
    // bd-k0p7: name the build that is ANSWERING, next to `core_v`. Without
    // this a stale installed binary is indistinguishable from a fresh one:
    // a committed doorbell-cli fix sat unrun for 11h because flywheel was
    // rebuilt and doorbell was not, and only an 8.9h-old stamp file gave it
    // away. The stamp is measured from the running exe, not the source tree.
    let build = running_build_stamp(now);
    // bd-k0p7: a build old enough to be suspect is DEGRADED, not merely
    // reported. The 11h incident is the reason this is not advisory: an
    // operator reading /health needs the daemon to say so, not to make the
    // subtraction themselves.
    let exe_mtime = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    if is_build_behind_source(exe_mtime, newest_doorbell_source_mtime()) {
        degraded.push("stale_build".to_string());
    }
    let ws_names: Vec<String> = hosted
        .iter()
        .filter_map(|h| hosted_workspace(h).map(str::to_string))
        .collect();
    let audits: Vec<std::path::PathBuf> = ws_names
        .iter()
        .map(|w| workspace_audit_path(&app.fw_state, w))
        .collect();
    let ages: Vec<Option<u64>> = audits
        .iter()
        .map(|p| {
            doorbell_rt::tailprobe::newest_match_age_secs(
                p.as_path(),
                "\"kind\":\"inbox_error\"",
                now,
            )
        })
        .collect();
    // Path-down only when EVERY hosted workspace has a fresh inbox_error.
    // One pin timeout (or one leftover Buffy host) is a local miss, not a
    // dead fetch path (bd-gpj0.4). The 2026-09-13 outage was every fetch.
    if inbox_path_down(&ages, PROBE_FRESH_SECS) {
        degraded.push("inbox_fetch_failing".to_string());
    }
    let journals: Vec<std::path::PathBuf> = ws_names
        .iter()
        .map(|w| workspace_journal_path(&app.fw_state, w))
        .collect();
    // bd-watch-restart-metric-8dvf: the brain journals one `RestartWatch` row
    // per ACTUAL restart (not per `Died` message), so this count is the thing
    // the flag's name says. It is also reported as a rate: a burst is legible
    // as N/min, where the bare threshold only says "crossed".
    let restarts = doorbell_rt::tailprobe::count_matches_within_across(
        journals.iter().map(std::path::PathBuf::as_path),
        "RestartWatch",
        now,
        WATCH_WINDOW_SECS,
    );
    let restart_budget = WATCH_RESTARTS_ALLOWED_PER_WS * ws_names.len();
    if restarts > restart_budget {
        degraded.push("watch_restart_loop".to_string());
    }
    // bd-ba8t: the prompt hop needs an INJECTABLE pane, and that precondition
    // used to be invisible — a hosted daemon with zero herdr agents read
    // `degraded: []` while no wake could ever land (gs-044's hard part).
    let inj = hosted_injection(&app.fw_state, &hosted, &bins.herdr);
    let no_prompt_hop = prompt_hop_degraded(hosted.len(), &inj);
    if no_prompt_hop {
        degraded.push("no_injectable_panes".to_string());
    }
    let mailbox_root = doorbell_rt::toron_data_root(None);
    let presence_age =
        doorbell_rt::watchbeat::file_age_secs(&doorbell_rt::presence_path(&mailbox_root));
    serde_json::json!({
        "protocol": doorbell_rt::api::PING.protocol,
        "core_v": doorbell_rt::api::PING.core_v,
        "journal_v": doorbell_rt::api::PING.journal_v,
        "uptime_s": app.started.elapsed().as_secs(),
        "mode": mode,
        "hosted": hosted,
        // bd-k0p7: the build identity of the RUNNING binary. `core_v` is a
        // compiled-in constant every build shares, so on its own it cannot
        // tell a stale installed binary from a fresh one; this names the build
        // that is actually answering.
        "build": build,
        // The CLIs actually exec'd. An operator reading `/health` after a
        // silent ring can see the resolved paths instead of guessing which
        // `toron`/`herdr` a launchd daemon found.
        "binaries": {
            "toron": bins.toron,
            "herdr": bins.herdr,
            "resolved": {
                "toron": bins.toron_resolved,
                "herdr": bins.herdr_resolved,
            },
        },
        // last_event_age_s is .event-only (GS-062 split; not hb). Quiet wake
        // (hb live, no bus lines) → null/stale here while wake.class=Quiet.
        // None also covers never-seen events (wrong bus or dead child).
        "watch": {
            "state": if hosted.is_empty() { "standby" } else { "hosted" },
            "last_event_age_s": watch_age,
            // bd-watch-restart-metric-8dvf: the measured rate next to the
            // budget it is compared against, so an operator reads "23.4/min
            // against a 1.2/min budget" instead of only "watch_restart_loop".
            "restarts": restarts,
            "restart_window_s": WATCH_WINDOW_SECS,
            "restart_budget_per_window": restart_budget,
            "restart_rate_per_min": ((restarts as f64 * 60.0 / WATCH_WINDOW_SECS as f64)
                * 100.0)
                .round()
                / 100.0,
        },
        // GS-062 ASK-USER E: wake_liveness Quiet/Live/Deaf (same shape as
        // flywheel serve /health). Keys come from AppState.hosted.
        "wake": wake,
        "sweep": {"last_age_s": sweep_age},
        // Measured, not hardcoded: the literal `0` here read as "no wake
        // subscriptions" even on a fully hosted daemon mid-ring, and sent
        // operators hunting the wrong fault.
        "subscribers": hosted_subscribers(&app.fw_state, &hosted),
        // The mailbox this daemon actually watches. A daemon on a different
        // toron root than the pins' bus is deaf; presence_age_s (None = no
        // projection ever written at the canonical root-level path) makes
        // that comparable in one read.
        "mailbox": {
            "toron_data_dir": mailbox_root.display().to_string(),
            "presence_age_s": presence_age,
        },
        // bd-ba8t: can a wake land here at all? `prompt_hop` is a word, not a
        // number to interpret: `no_pane_in_herdr` means every registry pin's
        // pane is unknown to herdr, so wakes queue/digest and never inject.
        "injection": {
            "pins": inj.pins,
            "panes_seen": inj.panes_seen,
            "injectable": inj.injectable,
            "panes_missing": inj.panes_missing,
            "prompt_hop": if no_prompt_hop { "no_pane_in_herdr" } else { "ok" },
        },
        "degraded": degraded,
        // bd-5qyt: the tear's SIZE rides beside the flag. `journal_torn_tail`
        // alone reads the same for a 16-row hole and a 5,175-row one, and this
        // body is what `doorbell status` prints verbatim. Explicit `null` (not
        // an empty array) when nothing is torn — the `standby_hint` shape, so
        // "nothing is torn" never arrives looking like "torn, but tiny".
        "torn_tails": if tears.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Array(
                tears
                    .iter()
                    .map(|(ws, t)| {
                        serde_json::json!({
                            "workspace": ws,
                            "file": t.file,
                            "at_line": t.at_line,
                            "reason": t.reason,
                            "missing_seqs": t.missing_seqs,
                            "missing_from": t.missing_from,
                            "missing_to": t.missing_to,
                            "rows_withheld": t.rows_withheld,
                        })
                    })
                    .collect(),
            )
        },
        "known_workspaces": known_workspaces.len(),
        "standby_hint": if wake_blind {
            serde_json::Value::String(
                "armed without --host: no wakes are delivered; re-arm with \
                 `doorbell start --host <project>/<workspace>`"
                    .to_string(),
            )
        } else {
            serde_json::Value::Null
        },
        "journal": {"seq": max_seq, "lag_lines": 0},
        "owner": {"pid": std::process::id(), "gen": null, "host": hostname()},
    })
    .to_string()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}
/// Retained journal rows as an SSE replay (D8, bd-asr2.12). Strictly
/// read-only fan-out over retained history plus a closing comment (no
/// infinite hold on an unchanging file); live push fan-out stays future.
fn events_body(state_dir: &std::path::Path, since: Option<u64>) -> String {
    use doorbell_rt::api::{Replay, SseEvent, replay_range};
    let store = doorbell_rt::FileStore::new(state_dir);
    let mut out = String::new();
    let mut workspaces: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(state_dir.join("journals")) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name
                .strip_prefix("doorbell-journal-")
                .and_then(|n| n.strip_suffix(".jsonl"))
                && !rest.is_empty()
                && !rest.contains('.')
            {
                workspaces.push(rest.to_string());
            }
        }
    }
    workspaces.sort();
    for ws in workspaces {
        let fold = store.fold_workspace(&ws);
        match replay_range(&fold.rows, fold.first_seq, since) {
            Replay::Events(events) => {
                for ev in events {
                    out.push_str(&ev.format());
                }
            }
            Replay::Gone { .. } => {}
        }
        // Retention honesty per workspace (same rule as `why`).
        if let (Some(q), Some(first)) = (since, fold.first_seq)
            && q < first.saturating_sub(1)
        {
            out.push_str(
                &SseEvent {
                    id: 0,
                    kind: "retention".into(),
                    data: format!("first replayable seq is {first}"),
                }
                .format(),
            );
        }
        if let Some(t) = fold.torn {
            out.push_str(
                &SseEvent {
                    id: 0,
                    kind: "torn".into(),
                    data: format!("fold stopped: {}", t.reason),
                }
                .format(),
            );
        }
    }
    out.push_str(": ready\n\n");
    out
}
/// Host-level counters (D8 stats shape; journal-derived truth across the
/// flywheel state dir; per-loop runtime counters stay in RunReports).
fn stats_body(app: &AppState) -> String {
    let hosted = hosted_keys(&app.hosted.lock());
    // bd-lorot: hoisted because `hosted` is MOVED into the json body below,
    // and the torn-tail check must be scoped to what we host.
    let hosted_ws: Vec<&str> = hosted.iter().filter_map(|h| hosted_workspace(h)).collect();
    let store = doorbell_rt::FileStore::new(&app.fw_state);
    let mut journal_rows = 0u64;
    let mut workspaces = 0u64;
    if let Ok(entries) = std::fs::read_dir(app.fw_state.join("journals")) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name
                .strip_prefix("doorbell-journal-")
                .and_then(|n| n.strip_suffix(".jsonl"))
                && !rest.is_empty()
                && !rest.contains('.')
            {
                workspaces += 1;
                journal_rows += store.journal_row_count(rest);
            }
        }
    }
    serde_json::json!({
        "protocol": doorbell_rt::api::PING.protocol,
        "core_v": doorbell_rt::api::PING.core_v,
        "journal_v": doorbell_rt::api::PING.journal_v,
        "uptime_s": app.started.elapsed().as_secs(),
        "mode": if hosted.is_empty() { "standby" } else { "hosted" },
        "hosted": hosted,
        // bd-k0p7: same stamp on the compact body, so neither /health shape
        // can be the one that hides a stale build.
        "build": running_build_stamp(doorbell_rt::lease::unix_now()),
        "workspaces": workspaces,
        "journal_rows": journal_rows,
        "torn_tail": store.any_torn_tail_among(&hosted_ws),
        "rings_by_class": {},
        "injects": 0,
        "drops": 0,
        "sheds": 0,
        "deaf": 0,
        "digest_batches": 0,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::known_journal_workspaces;
    use super::{PortHolder, classify_port_holder};

    /// The superseded wall-clock window, kept as a fixture: only the tests that
    /// describe what the age rule USED to degrade still need the number, and a
    /// production constant nobody reads is a build warning, not a policy.
    const LEGACY_BUILD_STALE_SECS: u64 = 6 * 60 * 60;

    /// bd-y40k: the drain's timeout verdict must SURFACE in the straggler
    /// note — a diagnosis nobody prints pins nothing.
    #[test]
    fn straggler_note_carries_the_liveness_verdict() {
        let report = super::DrainReport {
            joined: false,
            remaining: 1,
            total: 2,
            stragglers: vec!["flywheel/flywheel-flywheel".into()],
            verdicts: vec![(
                "flywheel/flywheel-flywheel".into(),
                "starved: last select wake 10200ms before the probe".into(),
            )],
        };
        let note = super::straggler_note(&report).expect("a straggler must note");
        assert!(note.contains("starved"), "verdict rendered: {note}");
        assert!(
            note.contains("flywheel/flywheel-flywheel"),
            "key still named: {note}"
        );
    }

    /// The timeout probe reads the SAME liveness the brain touches: a
    /// straggler that never woke during the drain window classifies
    /// starved, off the substituted clock (start_paused, no real wait).
    #[tokio::test(start_paused = true)]
    async fn drain_timeout_probes_straggler_liveness() {
        let liv = std::sync::Arc::new(doorbell_rt::brain::BrainLiveness::new());
        liv.touch();
        let mut entries = vec![super::LoopEntry {
            key: "stuck/one".into(),
            task: tokio::spawn(async { std::future::pending::<()>().await }),
            liveness: Some(liv),
        }];
        let report = super::drain_loops(&mut entries, std::time::Duration::from_secs(10)).await;
        assert!(!report.joined, "budget expires: {report:?}");
        assert_eq!(report.stragglers, ["stuck/one"]);
        assert_eq!(
            report.verdicts.len(),
            1,
            "one verdict per straggler: {report:?}"
        );
        assert_eq!(report.verdicts[0].0, "stuck/one");
        assert!(
            report.verdicts[0].1.contains("starved"),
            "no wake across the window: {}",
            report.verdicts[0].1
        );
    }

    /// bd-k0p7: the REAL resolver must produce a stamp with a real age. The
    /// pure resolver's tests would pass even if the IO half never called it,
    /// which is precisely the "correct helper, absent from the payload" trap.
    #[test]
    fn doorbell_health_running_build_stamp_reads_the_live_exe() {
        let now = doorbell_rt::lease::unix_now();
        let stamp = super::running_build_stamp(now);
        assert!(stamp.exe.is_some(), "the running exe is named: {stamp:?}");
        assert!(
            !stamp.version.is_empty(),
            "a stamp with no version names no build: {stamp:?}"
        );
        // Assert the IO half is WIRED and SELF-CONSISTENT, not that the binary
        // is fresh. The first version of this test asserted `age < 3600` on the
        // reasoning that "this test binary was linked seconds ago" — which is
        // true only on a cold build. On a warm cargo cache the binary is hours
        // old and the test went red (observed: 8037s) with nothing wrong in the
        // code. A test that passes only when the build cache is cold is not
        // hermetic; it is the same class as a test that reads ambient state.
        //
        // What actually needs proving is that the reported numbers describe the
        // SAME file the test can see. So read that file and compare.
        let exe = stamp.exe.clone().expect("stamp names an exe");
        let meta = std::fs::metadata(&exe)
            .unwrap_or_else(|e| panic!("the named exe must exist and be readable: {exe}: {e}"));
        let mtime = meta
            .modified()
            .expect("exe mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("post-epoch mtime")
            .as_secs();
        assert_eq!(
            stamp.exe_len,
            Some(meta.len()),
            "the reported size must be the named file's size"
        );
        let age = stamp.age_s.expect("a readable exe reports an age");
        let expected = now.saturating_sub(mtime);
        assert!(
            age.abs_diff(expected) <= 2,
            "the reported age must describe the named file: reported {age}s, \
             file is {expected}s old at the sampled `now` (2s tolerance for the \
             clock moving between the two reads)"
        );
    }

    /// bd-v7uu: `stale_build` must answer "is the build BEHIND a source change",
    /// not "has the build been up a long time".
    ///
    /// The old rule was `age_s > BUILD_STALE_SECS`, which is a proxy. Measured
    /// here: the only doorbell source change in the preceding 4.5h was a
    /// rustfmt-only commit, so the running daemon was equivalent to the tree and
    /// still reported `stale_build`. A field that fires on a healthy daemon
    /// trains the reader to skip it, and then it is skipped when it is true.
    ///
    /// Each case is falsifiable on its own, and the incident that motivated the
    /// field is still caught.
    #[test]
    fn stale_build_compares_source_not_the_clock() {
        use super::is_build_behind_source;

        // The shape that made this field exist: a committed fix sat unrun. Its
        // source IS newer than the exe, so it is still caught.
        assert!(
            is_build_behind_source(Some(1_000_000 - 39_600), Some(1_000_000)),
            "source newer than the exe IS a real staleness and must stay degraded"
        );

        // The false positive this replaces: a long-running daemon with NO newer
        // source. Under the old age rule this was degraded; it is not now.
        assert!(
            !is_build_behind_source(Some(1_000_000 - 39_600), Some(1_000_000 - 39_601)),
            "an 11h-old build with no source delta is UP TO DATE and must not degrade"
        );

        // Source exactly equal to the exe is not "behind".
        assert!(!is_build_behind_source(Some(1_000_000), Some(1_000_000)));

        // Unanswerable is not a fault: an unreadable exe, and a deployed binary
        // with no source tree beside it, must both stay quiet rather than cry wolf.
        assert!(!is_build_behind_source(None, Some(1_000_000)));
        assert!(!is_build_behind_source(Some(1_000_000), None));
        assert!(!is_build_behind_source(None, None));
    }

    /// The age is still published as ADVISORY, so the 11h incident's context
    /// survives — the fix changed what is DEGRADED, not what is visible.
    #[test]
    fn age_is_still_published_just_no_longer_degrading_on_its_own() {
        let old = doorbell_rt::api::resolve_build_stamp(Some((1_000_000 - 39_600, 1)), 1_000_000);
        assert_eq!(old.age_s, Some(39_600), "uptime stays visible");
        assert!(
            old.age_s.is_some_and(|a| a > LEGACY_BUILD_STALE_SECS),
            "and it still exceeds the advisory window, so the context is unchanged"
        );
    }

    /// bd-k0p7: the staleness rule keys off `age_s`, so a stamp with no age
    /// (unreadable exe) must NOT be treated as fresh AND must not be treated
    /// as stale — it is unknown, and the degrade check is written to say so.
    #[test]
    fn doorbell_health_unknown_age_is_neither_fresh_nor_stale() {
        let unknown = doorbell_rt::api::resolve_build_stamp(None, 1_000_000);
        assert_eq!(unknown.age_s, None);
        assert!(
            !unknown.age_s.is_some_and(|a| a > LEGACY_BUILD_STALE_SECS),
            "unknown must not read as stale"
        );
        // And a build inside the window is fresh.
        let fresh = doorbell_rt::api::resolve_build_stamp(Some((1_000_000 - 60, 1)), 1_000_000);
        assert!(!fresh.age_s.is_some_and(|a| a > LEGACY_BUILD_STALE_SECS));
        // The 11h incident shape IS stale.
        let stale = doorbell_rt::api::resolve_build_stamp(Some((1_000_000 - 39_600, 1)), 1_000_000);
        assert!(stale.age_s.is_some_and(|a| a > LEGACY_BUILD_STALE_SECS));
    }

    /// The `/health` shape seen for the first seconds after a restart:
    /// `standby`, no hosted loop, and a state dir that already knows
    /// workspaces. It reads like a failed restart and is not one — the daemon
    /// auto-hosts from subscriptions moments later.
    ///
    /// Pinned at the predicate so the boot window cannot be reported as a
    /// broken plane, and so a standby daemon over an empty state dir never
    /// borrows the flag to look degraded.
    #[test]
    fn a_standby_window_over_a_known_state_dir_is_degraded() {
        use super::standby_blind_to_wakes;

        assert!(
            standby_blind_to_wakes(0, 13),
            "standby over a state dir that knows workspaces delivers no wake, so it must degrade"
        );
        assert!(
            !standby_blind_to_wakes(2, 13),
            "a hosted daemon must never carry the standby flag"
        );
        assert!(
            !standby_blind_to_wakes(0, 0),
            "standby over an empty state dir is quiet, not degraded"
        );
    }

    /// bd-pf3b: the probe must cost ONE sample window for ALL stragglers —
    /// a serial sleep per key stacked N×200ms onto the drain budget and
    /// pushed total shutdown past stop's grace under load. Paused clock:
    /// serial costs budget + 3×SAMPLE, one window costs budget + 1×SAMPLE.
    #[tokio::test(start_paused = true)]
    async fn drain_probes_every_straggler_in_one_window() {
        let mk = |key: &str| super::LoopEntry {
            key: key.into(),
            task: tokio::spawn(async { std::future::pending::<()>().await }),
            liveness: Some(std::sync::Arc::new(doorbell_rt::brain::BrainLiveness::new())),
        };
        let mut entries = vec![mk("a/a"), mk("b/b"), mk("c/c")];
        let budget = std::time::Duration::from_secs(10);
        let started = tokio::time::Instant::now();
        let report = super::drain_loops(&mut entries, budget).await;
        let elapsed = started.elapsed();
        assert!(!report.joined);
        assert_eq!(report.verdicts.len(), 3, "every straggler classified");
        assert!(
            elapsed <= budget + super::LIVENESS_PROBE_SAMPLE,
            "probe must not stack per-key windows: {elapsed:?} > {:?}",
            budget + super::LIVENESS_PROBE_SAMPLE
        );
    }

    #[test]
    fn drain_note_never_calls_a_budget_exhaustion_a_clean_join() {
        let joined = super::drain_note(true, 0, 6);
        assert!(joined.contains("loops joined"), "{joined}");
        let hung = super::drain_note(false, 3, 6);
        assert!(hung.contains("DRAIN BUDGET EXCEEDED"), "{hung}");
        assert!(hung.contains("3 of 6"), "{hung}");
    }

    /// bd-zzgb: the old drain awaited handles HEAD-OF-LINE and decremented
    /// only as each await returned, so one hung brain reported "6 of 6 did
    /// not join" while the loops behind it had already exited. Stragglers
    /// must be read from the handles, never from a decrement counter.
    #[tokio::test(start_paused = true)]
    async fn drain_counts_a_finished_loop_even_when_one_never_joins() {
        let mut entries = vec![
            super::LoopEntry {
                key: "fast/flywheel".into(),
                task: tokio::spawn(async {}),
                liveness: None,
            },
            super::LoopEntry {
                key: "stuck/flywheel".into(),
                task: tokio::spawn(async { std::future::pending::<()>().await }),
                liveness: None,
            },
        ];
        let report = super::drain_loops(&mut entries, std::time::Duration::from_secs(10)).await;
        assert!(!report.joined, "budget must expire: {report:?}");
        assert_eq!(
            (report.remaining, report.total),
            (1, 2),
            "only the stuck loop may remain: {report:?}"
        );
        assert_eq!(report.stragglers, ["stuck/flywheel"]);
    }

    #[tokio::test(start_paused = true)]
    async fn drain_is_a_clean_join_when_every_loop_finishes() {
        let mut entries = vec![
            super::LoopEntry {
                key: "a/a".into(),
                task: tokio::spawn(async {}),
                liveness: None,
            },
            super::LoopEntry {
                key: "b/b".into(),
                task: tokio::spawn(async {}),
                liveness: None,
            },
        ];
        let report = super::drain_loops(&mut entries, std::time::Duration::from_secs(10)).await;
        assert!(
            report.joined && report.remaining == 0 && report.stragglers.is_empty(),
            "{report:?}"
        );
        assert!(
            super::drain_note(report.joined, report.remaining, report.total)
                .contains("loops joined")
        );
    }

    /// bd-burk's second discriminator: a Shutdown whose channel was already
    /// closed (stale hosted row) used to be swallowed by `let _ =` — dead
    /// indistinguishable from a brain still chewing its backlog. The failed
    /// keys come back as VALUES; the caller makes them loud.
    #[tokio::test]
    async fn shutdown_send_names_loops_whose_channel_is_already_closed() {
        let (live_tx, mut live_rx) = tokio::sync::mpsc::channel(4);
        let (dead_tx, dead_rx) = tokio::sync::mpsc::channel(4);
        drop(dead_rx);
        let retiring = vec![
            super::HostedLoop {
                key: "alive/ws".into(),
                tx: live_tx,
            },
            super::HostedLoop {
                key: "stale/ws".into(),
                tx: dead_tx,
            },
        ];
        let failed = super::send_shutdowns(&retiring, std::time::Duration::from_secs(2)).await;
        assert_eq!(
            failed,
            [("stale/ws".to_string(), super::SendFail::ChannelClosed)],
            "only the closed channel fails"
        );
        assert!(
            matches!(live_rx.try_recv(), Ok(doorbell_rt::BrainMsg::Shutdown)),
            "the live loop still received Shutdown"
        );
    }

    /// bd-umuh: the send phase ran unbounded ahead of the bounded drain —
    /// one brain not receiving (mid `run_cli`, up to 15s) parked the serial
    /// walk, the 10s drain budget never even started, and stop's grace
    /// expired mid-drain (SIGKILL before any note printed). The budget is
    /// injected policy: BOTH sends here are genuinely blocked (each channel
    /// is already full), so both report the exhausted budget and the call
    /// still returns inside it. bd-v5r0 kept this shape, and added the
    /// regression test above it for the case where only the first is blocked.
    #[tokio::test(start_paused = true)]
    async fn send_shutdowns_bounds_the_send_phase_and_names_every_unsent_key() {
        // Capacity 1, pre-filled: the next send parks until the budget fires.
        let (blocked_tx, _blocked_rx) = tokio::sync::mpsc::channel(1);
        blocked_tx
            .try_send(doorbell_rt::BrainMsg::Shutdown)
            .unwrap();
        let (behind_tx, _behind_rx) = tokio::sync::mpsc::channel(1);
        behind_tx.try_send(doorbell_rt::BrainMsg::Shutdown).unwrap();
        let retiring = vec![
            super::HostedLoop {
                key: "busy/ws".into(),
                tx: blocked_tx,
            },
            super::HostedLoop {
                key: "queued/ws".into(),
                tx: behind_tx,
            },
        ];
        let failed = super::send_shutdowns(&retiring, std::time::Duration::from_millis(500)).await;
        assert_eq!(
            failed,
            [
                ("busy/ws".to_string(), super::SendFail::BudgetExhausted),
                ("queued/ws".to_string(), super::SendFail::BudgetExhausted),
            ],
            "blocked key and the never-attempted key behind it both report the exhausted budget: {failed:?}"
        );
    }

    /// bd-v5r0: one blocked send must not starve the loops behind it. The
    /// walk was sequential under a single shared deadline, so a full channel
    /// (a brain mid-dispatch) parked the walk and every key behind it was
    /// reported BudgetExhausted WITHOUT being tried — the same head-of-line
    /// defect the join phase was already cured of, which is why this is a
    /// regression test and not an observation.
    ///
    /// Fixture: the FIRST loop is full (its send parks) and the SECOND is
    /// empty (its send lands on the first poll). Racing the sends tells the
    /// empty one; walking in table order never reaches it.
    #[tokio::test(start_paused = true)]
    async fn send_shutdowns_races_so_one_blocked_brain_cannot_starve_the_rest() {
        let (busy_tx, _busy_rx) = tokio::sync::mpsc::channel(1);
        busy_tx.try_send(doorbell_rt::BrainMsg::Shutdown).unwrap();
        let (free_tx, mut free_rx) = tokio::sync::mpsc::channel(1);
        let retiring = vec![
            super::HostedLoop {
                key: "busy/ws".into(),
                tx: busy_tx,
            },
            super::HostedLoop {
                key: "free/ws".into(),
                tx: free_tx,
            },
        ];
        let failed = super::send_shutdowns(&retiring, std::time::Duration::from_millis(500)).await;
        assert_eq!(
            failed,
            [("busy/ws".to_string(), super::SendFail::BudgetExhausted)],
            "only the loop whose OWN send could not land fails: {failed:?}"
        );
        assert!(
            matches!(free_rx.try_recv(), Ok(doorbell_rt::BrainMsg::Shutdown)),
            "the loop behind the blocked one MUST have been told — a sequential \
             walk under one deadline never tried it"
        );
    }

    #[test]
    fn straggler_note_names_every_loop_that_missed_the_deadline() {
        let stuck = super::DrainReport {
            joined: false,
            remaining: 2,
            total: 3,
            stragglers: vec!["p/one".into(), "p/two".into()],
            verdicts: Vec::new(),
        };
        let note = super::straggler_note(&stuck).expect("stuck loops must be named");
        assert!(note.contains("p/one") && note.contains("p/two"), "{note}");
        let clean = super::DrainReport {
            joined: true,
            remaining: 0,
            total: 3,
            stragglers: vec![],
            verdicts: Vec::new(),
        };
        assert!(super::straggler_note(&clean).is_none());
    }
    use super::{
        REAP_AFTER_SECS, bins_allow_hosting, inbox_path_down, reap_candidates, unhosted_subscribed,
    };

    /// bd-xwgh round 2: a mid-shutdown doorbell whose accept loop is gone
    /// still accepts into the backlog but never answers — it must read as
    /// Unresponsive (exit 0, stay dead), never as the old exit-1
    /// "non-doorbell squatter" bail that respawn-warred the launchd unit
    /// (runs=69, watch_restart_loop).
    #[test]
    fn port_holder_distinguishes_peer_unresponsive_and_foreign() {
        let peer =
            serde_json::json!({"protocol": doorbell_rt::api::PING.protocol, "owner": {"pid": 1}});
        let foreign = serde_json::json!({"hello": "world"});
        assert_eq!(classify_port_holder(Some(&peer)), PortHolder::Peer);
        assert_eq!(classify_port_holder(None), PortHolder::Unresponsive);
        assert_eq!(classify_port_holder(Some(&foreign)), PortHolder::Foreign);
    }

    /// bd-fuql: hosting with either CLI unresolvable is refused — a deaf
    /// plane must never read as hosted.
    #[test]
    fn boot_gate_refuses_unresolved_bins() {
        assert!(bins_allow_hosting(true, true));
        assert!(!bins_allow_hosting(false, true), "toron missing refuses");
        assert!(!bins_allow_hosting(true, false), "herdr missing refuses");
        assert!(!bins_allow_hosting(false, false));
    }

    /// bd-gpj0.4: one workspace with a fresh path-error is a local miss.
    /// The 2026-09-13 outage was every hosted fetch failing.
    #[test]
    fn inbox_path_down_requires_every_hosted_workspace() {
        assert!(
            !inbox_path_down(&[], 120),
            "standby (no hosts) is not a fetch outage"
        );
        assert!(
            inbox_path_down(&[Some(5)], 120),
            "a single hosted workspace with a fresh error is path-down"
        );
        assert!(
            !inbox_path_down(&[Some(5), None], 120),
            "a sibling without a path-error keeps the daemon honest"
        );
        assert!(
            inbox_path_down(&[Some(5), Some(8)], 120),
            "every hosted workspace failing is the original outage"
        );
        assert!(
            !inbox_path_down(&[Some(500)], 120),
            "a stale error is not live path-down"
        );
    }

    /// bd-mk60 test seam: a keyed host handle with a real (unused) channel.
    /// `tokio::sync::mpsc::channel` needs no runtime, so this works from a
    /// plain `#[test]`.
    fn host_for_test(key: &str) -> super::HostedLoop {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        super::HostedLoop {
            key: key.to_string(),
            tx,
        }
    }

    #[test]
    fn dead_worker_removal_is_keyed_and_invalidates_health() {
        let hosted: super::Hosts = std::sync::Arc::new(parking_lot::Mutex::new(vec![
            host_for_test("proj/live"),
            host_for_test("proj/dead"),
        ]));
        let health = std::sync::Arc::new(parking_lot::Mutex::new(super::CachedHealth {
            body: "cached".into(),
            computed_at: std::time::Instant::now(),
        }));
        assert!(super::remove_host(&hosted, "proj/dead"));
        assert!(!super::remove_host(&hosted, "proj/missing"));
        assert_eq!(super::hosted_keys(&hosted.lock()), vec!["proj/live"]);
        super::invalidate_health_snapshot(&health);
        assert!(
            health.lock().computed_at.elapsed().as_secs() > super::HEALTH_CACHE_TTL_SECS,
            "worker exit must force the next /health request to recompute"
        );
    }

    #[test]
    fn hosted_keys_for_health_orders_by_workspace_slug() {
        // Insertion order: beta then alpha; health order must be alpha first.
        let hosts = vec![host_for_test("zzz/beta"), host_for_test("aaa/alpha")];
        let keys = super::hosted_keys(&hosts);
        assert_eq!(
            keys[0], "zzz/beta",
            "insertion order preserved for reconcile"
        );
        let health = super::hosted_keys_for_health(&hosts);
        assert_eq!(
            health,
            vec!["aaa/alpha".to_string(), "zzz/beta".to_string()],
            "health primary follows alphabetical workspace slug"
        );
    }

    #[test]
    fn hosted_wake_pipe_empty_is_never_stamped() {
        let dir = std::env::temp_dir().join(format!(
            "db-wake-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let wake = super::hosted_wake_pipe(&dir, &[]);
        assert_eq!(wake["pipe"]["class"], "NeverStamped", "{wake}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hosted_wake_pipe_live_only_is_live() {
        let dir = std::env::temp_dir().join(format!(
            "db-wake-live-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(dir.join("doorbell")).expect("dir");
        doorbell_rt::watchbeat::touch_event(&dir, "live-ws");
        doorbell_rt::watchbeat::touch_heartbeat(&dir, "live-ws");
        let wake = super::hosted_wake_pipe(&dir, &["proj/live-ws".into()]);
        assert_eq!(wake["pipe"]["class"], "Live", "{wake}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hosted_wake_pipe_worst_of_deaf_plus_live() {
        // Stamp-pair unit (mtime backdate not required): Deaf + Live → Deaf.
        let pipe = doorbell_rt::wake_liveness::classify_worst_of(
            200_000,
            [
                (None, Some(100_000u64)),             // Deaf
                (Some(199_000u64), Some(198_000u64)), // Live
            ],
            doorbell_rt::wake_liveness::DEFAULT_DEAF_CEILING_MS,
        );
        assert_eq!(
            pipe,
            doorbell_rt::wake_liveness::PipeHealth::Deaf {
                silent_for_ms: 100_000
            },
            "ask-user F: Deaf must win over Live"
        );
    }

    #[test]
    fn hosted_wake_pipe_worst_workspace_label_matches_stamps() {
        // Winner label from classify_worst_of_stamps must be the Deaf ws, not
        // first-alive Live — same contract hosted_wake_pipe uses for workspace.
        let (pipe, winner, ev, hb) = doorbell_rt::wake_liveness::classify_worst_of_stamps(
            200_000,
            [
                ("live-ws", Some(199_000u64), Some(198_000u64)),
                ("deaf-ws", None, Some(100_000u64)),
            ],
            doorbell_rt::wake_liveness::DEFAULT_DEAF_CEILING_MS,
        );
        assert!(matches!(
            pipe,
            doorbell_rt::wake_liveness::PipeHealth::Deaf { .. }
        ));
        assert_eq!(winner, Some("deaf-ws"));
        assert_eq!((ev, hb), (None, Some(100_000)));
    }

    /// bd-mk60: reconcile retires a loop whose subscription went away, and
    /// ONLY that one. A boot-configured host (`--host`) is an explicit
    /// operator choice that outlives any subscription, and a still-subscribed
    /// workspace keeps its loop.
    #[test]
    fn retire_keys_drops_only_unsubscribed_non_boot_hosts() {
        let boot = vec!["proj/boot".to_string()];
        let hosted = vec![
            "proj/boot".to_string(),
            "proj/live".to_string(),
            "proj/gone".to_string(),
        ];
        let live = vec!["proj/live".to_string()];
        assert_eq!(
            super::retire_keys(&hosted, &boot, &live),
            vec!["proj/gone".to_string()],
            "only the unsubscribed, non-boot host retires"
        );
        // Nothing subscribed and nothing boot-configured: every loop retires.
        assert_eq!(
            super::retire_keys(&hosted, &[], &[]).len(),
            3,
            "an empty subscription set retires every non-boot host"
        );
        // A boot host is never retired on its own account.
        assert!(super::retire_keys(&boot, &boot, &[]).is_empty());
    }

    /// bd-gpj0.1: `flywheel arrive . --as Buffy` wrote project `.` /
    /// workspace `flywheel-.`. Auto-hosting that pair sweeps a pin with
    /// no key and paints the WHOLE daemon `inbox_fetch_failing` while
    /// real CLI inboxes work. Unaddressable projects are not live keys.
    #[test]
    fn subscribed_keys_skip_dot_and_empty_projects() {
        use doorbell_rt::subscribe::*;
        let mk = |id: &str, project: Option<&str>| Subscription {
            id: id.into(),
            identity: Identity::Pin("Buffy".into()),
            scope: Scope::One("flywheel-.".into()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
            project: project.map(str::to_string),
            endpoint: None,
        };
        let keys = super::subscribed_keys(&[
            mk("cli-dot", Some(".")),
            mk("cli-empty", Some("")),
            mk("cli-dotdot", Some("..")),
            mk("cli-ok", Some("flywheel")),
            mk("cli-legacy", None),
        ]);
        assert_eq!(
            keys,
            vec!["flywheel/flywheel-.".to_string()],
            "only a real slug auto-hosts, got {keys:?}"
        );
    }

    /// The key a subscription names is built in ONE place, so the auto-host
    /// rule and the retire rule cannot disagree about what is referenced:
    /// what `subscribed_keys` names is exactly what `retire_keys` treats as
    /// still-live.
    #[test]
    fn retire_key_reads_the_same_subscription_set_that_auto_hosts() {
        use doorbell_rt::subscribe::*;
        let named = Subscription {
            id: "spawn-w-p".into(),
            identity: Identity::Pin("p".into()),
            scope: Scope::One("w".into()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
            project: Some("proj".into()),
            endpoint: None,
        };
        let live = super::subscribed_keys(&[named]);
        assert_eq!(live, vec!["proj/w".to_string()]);
        let hosted = vec!["proj/w".to_string(), "proj/orphan".to_string()];
        // Even with no boot config, the subscribed key survives and the
        // orphan does not — the two rules read one set.
        assert_eq!(
            super::retire_keys(&hosted, &[], &live),
            vec!["proj/orphan".to_string()]
        );
    }

    /// bd-enov (F17): the wake-blind detector reads exactly the workspaces the
    /// plane has served — a journal file per workspace, ignoring strays and
    /// atomic-write temp files.
    #[test]
    fn known_journal_workspaces_reads_the_plane_scope() {
        let dir = std::env::temp_dir().join(format!("db-known-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("journals")).expect("mkdir");
        assert!(
            known_journal_workspaces(&dir).is_empty(),
            "no journals -> no known workspaces"
        );
        std::fs::write(
            dir.join("journals")
                .join("doorbell-journal-flywheel-toron.jsonl"),
            b"",
        )
        .expect("write journal");
        std::fs::write(
            dir.join("journals")
                .join("doorbell-journal-flywheel-toron.jsonl.tmp"),
            b"",
        )
        .expect("write tmp");
        std::fs::write(dir.join("journals").join("stray.txt"), b"").expect("write stray");
        assert_eq!(
            known_journal_workspaces(&dir),
            vec!["flywheel-toron".to_string()],
            "only live journal names count"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-2o1v: subscriptions are the hosting truth — named (project, ws)
    /// pairs not yet hosted come back; legacy project-less rows and
    /// already-hosted keys never do.
    #[test]
    fn unhosted_subscribed_names_only_nameable() {
        use doorbell_rt::subscribe::*;
        let named = Subscription {
            id: "spawn-w-p".into(),
            identity: Identity::Pin("p".into()),
            scope: Scope::One("w".into()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
            project: Some("proj".into()),
            endpoint: None,
        };
        let legacy = Subscription {
            project: None,
            ..named.clone()
        };
        assert_eq!(
            unhosted_subscribed(&[named.clone(), legacy], &[]),
            vec![("proj".to_string(), "w".to_string())],
            "legacy rows without a project are never guessed into a host"
        );
        assert!(
            unhosted_subscribed(&[named], &["proj/w".to_string()]).is_empty(),
            "already-hosted keys stay hosted, never double-spawned"
        );
    }

    /// bd-2o1v: the reaper drops spawn-scoped rows after a week of journal
    /// silence; fresh journals, journal-less workspaces, and operator rows
    /// stay. Missing journal = fresh spawn pre-first-tick, never a corpse.
    /// bd-ba8t: `herdr agent list` = [] while the registry names pins is the
    /// live condition that made the ring unable to land and `/health` look
    /// clean. It must degrade; a busy fleet (panes seen, none free) must not.
    #[test]
    fn prompt_hop_degrades_only_when_herdr_knows_no_pane() {
        use super::prompt_hop_degraded;
        use doorbell_rt::file_host::InjectionPicture;
        let pic = |pins, seen, injectable, missing| InjectionPicture {
            pins,
            panes_seen: seen,
            injectable,
            panes_missing: missing,
        };
        assert!(
            prompt_hop_degraded(1, &pic(3, 0, 0, 3)),
            "three pins, herdr knows none of their panes"
        );
        assert!(
            !prompt_hop_degraded(1, &pic(3, 3, 0, 0)),
            "all panes busy is healthy: the queue drains on the next idle"
        );
        assert!(
            !prompt_hop_degraded(0, &pic(0, 0, 0, 0)),
            "standby hosts nothing"
        );
        assert!(
            !prompt_hop_degraded(1, &pic(0, 0, 0, 0)),
            "hosted with an empty registry is not a prompt-hop fault"
        );
    }

    #[test]
    fn reap_candidates_only_quiet_spawn_rows() {
        use doorbell_rt::subscribe::*;
        let spawn = |id: &str, ws: &str| Subscription {
            id: id.into(),
            identity: Identity::Pin("p".into()),
            scope: Scope::One(ws.into()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
            project: Some("proj".into()),
            endpoint: None,
        };
        let cli = spawn("cli-w-p", "w");
        let rows = vec![
            spawn("spawn-old-p", "old"),
            spawn("spawn-fresh-p", "fresh"),
            spawn("spawn-never-p", "never"),
            cli,
        ];
        let aged = |ws: &str| match ws {
            "old" => Some(REAP_AFTER_SECS + 1),
            "fresh" => Some(10),
            _ => None,
        };
        assert_eq!(
            reap_candidates(&rows, aged),
            vec!["spawn-old-p".to_string()]
        );
    }

    /// bd-8ulu: the daemon registers exactly the webhook adapters the
    /// workspace's rows justify, and NOTHING for a row it cannot honour.
    /// Registering a sink nobody's row names, or skipping a sink a row DOES
    /// name, are both the same class of bug: a wake that reports delivered
    /// and lands nowhere.
    #[test]
    fn reconcile_webhook_registrations_follow_the_rows_and_fail_closed() {
        use doorbell_rt::sinks::EgressAllowlist;
        use doorbell_rt::subscribe::*;
        let allow = EgressAllowlist::new(["127.0.0.1".to_string()]);
        let row = |id: &str, sink: SinkKind, url: Option<&str>, ws: &str| Subscription {
            id: id.into(),
            identity: Identity::Pin(format!("Pin-{id}")),
            scope: Scope::One(ws.into()),
            filter: Filter::default(),
            sink,
            budget: Budget::default_530(),
            project: Some("proj".into()),
            endpoint: url.map(str::to_string),
        };
        let named = row(
            "a",
            SinkKind::Webhook,
            Some("http://127.0.0.1:18999/wake"),
            "w",
        );
        let (regs, refused) =
            super::webhook_registrations(std::slice::from_ref(&named), "w", &allow);
        assert_eq!(
            regs,
            vec![super::WebhookReg {
                route: "webhook:http://127.0.0.1:18999/wake".into(),
                url: "http://127.0.0.1:18999/wake".into(),
            }],
            "a named, allowlisted endpoint registers under its row's route"
        );
        assert!(refused.is_empty(), "{refused:?}");

        // Two rows, same endpoint -> ONE adapter (a second would be a
        // duplicate worker for the same socket).
        let (regs, _) = super::webhook_registrations(&[named.clone(), named.clone()], "w", &allow);
        assert_eq!(regs.len(), 1, "distinct endpoints, not distinct rows");

        // Two rows, two endpoints -> TWO adapters (that is the point of
        // per-subscription routing).
        let other = row(
            "b",
            SinkKind::Webhook,
            Some("http://127.0.0.1:19000/wake"),
            "w",
        );
        let (regs, _) = super::webhook_registrations(&[named.clone(), other], "w", &allow);
        assert_eq!(regs.len(), 2, "per-subscription endpoints stay separate");

        // A webhook row with no URL: refuse LOUDLY, register nothing.
        let bare = row("c", SinkKind::Webhook, None, "w");
        let (regs, refused) = super::webhook_registrations(&[bare], "w", &allow);
        assert!(regs.is_empty(), "no URL means no adapter: {regs:?}");
        assert_eq!(refused.len(), 1, "and it must be named: {refused:?}");
        assert!(refused[0].contains("--url"), "{refused:?}");

        // An off-allowlist endpoint: refused (D6 egress), nothing registered.
        let evil = row(
            "d",
            SinkKind::Webhook,
            Some("http://evil.example/wake"),
            "w",
        );
        let (regs, refused) = super::webhook_registrations(&[evil], "w", &allow);
        assert!(regs.is_empty(), "egress is allowlisted or absent: {regs:?}");
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(
            refused[0].contains(doorbell_rt::sinks::EGRESS_ALLOWLIST_ENV),
            "the refusal names the operator knob: {refused:?}"
        );

        // Another workspace's rows are not this host's business, and a
        // herdr row never becomes a webhook adapter.
        let elsewhere = row(
            "e",
            SinkKind::Webhook,
            Some("http://127.0.0.1:19100/wake"),
            "other",
        );
        let herdr = row(
            "f",
            SinkKind::Herdr,
            Some("http://127.0.0.1:19200/wake"),
            "w",
        );
        let (regs, refused) = super::webhook_registrations(&[elsewhere, herdr], "w", &allow);
        assert!(regs.is_empty(), "scope + sink kind both gate: {regs:?}");
        assert!(refused.is_empty(), "neither is an error: {refused:?}");
    }

    /// The 2026-09-13 outage was invisible in the one place an operator
    /// looks: every inbox fetch failed (`inbox_error` per pin per second)
    /// and the watch child thrashed (a `RestartWatch` every 16s), yet
    /// `/health` reported `degraded: []`. Both facts are now read back from
    /// the ground-truth surfaces and named.
    #[test]
    fn health_names_fetch_failure_and_watch_thrash() {
        use super::{AppState, CachedHealth, health_body};
        let dir = std::env::temp_dir().join(format!("db-health-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("doorbell")).expect("mkdir audit");
        std::fs::create_dir_all(dir.join("journals")).expect("mkdir journal");
        let ts = chrono::DateTime::from_timestamp(doorbell_rt::lease::unix_now() as i64 - 5, 0)
            .expect("now in range")
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        std::fs::write(
            dir.join("doorbell/flywheel-flywheel.audit.jsonl"),
            format!(
                "{{\"ts\":\"{ts}\",\"kind\":\"inbox_error\",\"workspace\":\"flywheel-flywheel\"}}\n"
            ),
        )
        .expect("write audit");
        // Thrash = a restart every 16s (the live failure), not one routine
        // idle-exit restart: the probe must count, not sample recency.
        let mut journal = String::new();
        for off in (0..=288).step_by(16) {
            let stamp =
                chrono::DateTime::from_timestamp(doorbell_rt::lease::unix_now() as i64 - off, 0)
                    .expect("in range")
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            journal.push_str(&format!(
                "{{\"ts\":\"{stamp}\",\"v\":2,\"actions\":[\"RestartWatch\"]}}\n"
            ));
        }
        std::fs::write(
            dir.join("journals/doorbell-journal-flywheel-flywheel.jsonl"),
            journal,
        )
        .expect("write journal");
        let app = AppState {
            started: std::time::Instant::now(),
            fw_state: dir.clone(),
            hosted: std::sync::Arc::new(parking_lot::Mutex::new(vec![host_for_test(
                "flywheel/flywheel-flywheel",
            )])),
            health: std::sync::Arc::new(parking_lot::Mutex::new(CachedHealth {
                body: String::new(),
                computed_at: std::time::Instant::now(),
            })),
        };
        let body = health_body(&app);
        assert!(
            body.contains("inbox_fetch_failing"),
            "a failing inbox fetch must be named in degraded: {body}"
        );
        assert!(
            body.contains("watch_restart_loop"),
            "watch thrash must be named in degraded: {body}"
        );
        // bd-watch-restart-metric-8dvf: 19 rows inside the 300s window is 3.8
        // restarts/min against a 6-per-window budget. Both numbers ride in the
        // payload, so a burst is legible as a rate rather than only as a
        // crossed flag.
        assert!(
            body.contains("\"restarts\":19") && body.contains("\"restart_rate_per_min\":3.8"),
            "the measured rate must be readable next to the flag: {body}"
        );
        assert!(
            body.contains("\"restart_budget_per_window\":6"),
            "the budget the burst was measured against must be named: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tear detail exists (`bd-9ase`) and `/health` still reduced it to one
    /// bare name: `degraded: ["journal_torn_tail"]` reads identically for a
    /// 16-row hole and a 5,175-row one. The numbers ride in `torn_tails` now, so
    /// `doorbell status` — which prints this body verbatim — answers "how bad"
    /// in the same probe.
    ///
    /// The ADR-0012 token stays in `degraded` (E14 and the CLI reader pin it);
    /// the detail is ADDED, never substituted.
    #[test]
    fn health_names_the_tear_size_not_just_the_kind() {
        use super::{AppState, CachedHealth, health_body};
        let dir = std::env::temp_dir().join(format!("db-health-tear-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("doorbell")).expect("mkdir audit");
        std::fs::create_dir_all(dir.join("journals")).expect("mkdir journal");
        // Sweeps contiguous through seq 3, then seq 10: six values (4..=9) gone,
        // and the two rows after the hole are what the fold withholds.
        let mut journal = String::new();
        for seq in [1, 2, 3, 10, 11] {
            journal.push_str(&format!(
                "{{\"v\":2,\"seq\":{seq},\"actions\":[],\"executed\":{{}}}}\n"
            ));
        }
        std::fs::write(
            dir.join("journals/doorbell-journal-flywheel-flywheel.jsonl"),
            journal,
        )
        .expect("write journal");
        let app = AppState {
            started: std::time::Instant::now(),
            fw_state: dir.clone(),
            hosted: std::sync::Arc::new(parking_lot::Mutex::new(vec![host_for_test(
                "flywheel/flywheel-flywheel",
            )])),
            health: std::sync::Arc::new(parking_lot::Mutex::new(CachedHealth {
                body: String::new(),
                computed_at: std::time::Instant::now(),
            })),
        };
        let body = health_body(&app);
        // The flag itself is unchanged: still degraded, still the pinned name.
        assert!(
            body.contains("journal_torn_tail"),
            "a real tear still degrades with the ADR-0012 token: {body}"
        );
        // And now the numbers that make it readable: which workspace, how many
        // seqs, which range, and how much history the fold cannot reach.
        assert!(
            body.contains("\"workspace\":\"flywheel-flywheel\"")
                && body.contains("\"missing_seqs\":6")
                && body.contains("\"missing_from\":4")
                && body.contains("\"missing_to\":9")
                && body.contains("\"rows_withheld\":2"),
            "the tear must arrive with its size, range and withheld rows: {body}"
        );
        // A healthy workspace must NOT grow the key: an always-present empty
        // array reads as a field that means nothing.
        std::fs::write(
            dir.join("journals/doorbell-journal-flywheel-flywheel.jsonl"),
            "{\"v\":2,\"seq\":1,\"actions\":[],\"executed\":{}}\n",
        )
        .expect("heal journal");
        let healed = health_body(&app);
        assert!(
            !healed.contains("journal_torn_tail"),
            "a healed journal is not degraded: {healed}"
        );
        assert!(
            healed.contains("\"torn_tails\":null"),
            "and it says so explicitly rather than with an empty array: {healed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pin with no key is not a fetch-path outage (bd-gpj0.1). Buffy on
    /// project `.` wrote `inbox_error` every sweep and kept
    /// `inbox_fetch_failing` on while CLI mail worked.
    #[test]
    fn health_does_not_treat_pin_miss_as_fetch_path_down() {
        use super::{AppState, CachedHealth, health_body};
        let dir = std::env::temp_dir().join(format!("db-health-pinmiss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("doorbell")).expect("mkdir audit");
        let ts = chrono::DateTime::from_timestamp(doorbell_rt::lease::unix_now() as i64 - 5, 0)
            .expect("now in range")
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        std::fs::write(
            dir.join("doorbell/flywheel-..audit.jsonl"),
            format!(
                "{{\"ts\":\"{ts}\",\"kind\":\"inbox_pin_miss\",\"workspace\":\"flywheel-.\"}}\n"
            ),
        )
        .expect("write audit");
        let app = AppState {
            started: std::time::Instant::now(),
            fw_state: dir.clone(),
            hosted: std::sync::Arc::new(parking_lot::Mutex::new(vec![host_for_test(
                "./flywheel-.",
            )])),
            health: std::sync::Arc::new(parking_lot::Mutex::new(CachedHealth {
                body: String::new(),
                computed_at: std::time::Instant::now(),
            })),
        };
        let body = health_body(&app);
        assert!(
            !body.contains("inbox_fetch_failing"),
            "pin-miss must not paint the fetch path down: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-o86s RED: /health serves a cached snapshot — a fresh cache
    /// returns its body without recomputing (no shells, no greps).
    #[test]
    fn health_cache_serves_snapshot_without_recompute() {
        use super::{AppState, CachedHealth, HEALTH_CACHE_TTL_SECS, cached_health_body};
        use std::time::Instant;
        let dir = std::env::temp_dir().join(format!("db-healthcache-{}", std::process::id()));
        let app = AppState {
            started: Instant::now(),
            fw_state: dir,
            hosted: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            health: std::sync::Arc::new(parking_lot::Mutex::new(CachedHealth {
                body: "CANNED".to_string(),
                computed_at: Instant::now(),
            })),
        };
        assert_eq!(cached_health_body(&app), "CANNED");
        const {
            assert!(
                HEALTH_CACHE_TTL_SECS <= 30,
                "a health snapshot older than a sweep era lies"
            );
        }
    }
}

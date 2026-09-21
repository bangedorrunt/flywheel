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
    // Watch child (CLI; the in-process bus needs toron-core, which the
    // lean daemon never links). Backoff + retry gate live in the host.
    host.restart_watch();
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
    let brain = Brain::new(BrainConfig::new(&workspace), host, store, sinks);
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

/// The hosted keys, in insertion order — what `/health` reports as `hosted`.
fn hosted_keys(hosts: &[HostedLoop]) -> Vec<String> {
    hosts.iter().map(|h| h.key.clone()).collect()
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
            // D9 layer 2: healthy doorbell peer → clean exit 0 (no respawn
            // war under Restart=on-failure); anyone else → hard fail naming
            // the squatter (toron serve_lock precedent).
            match peer_health(cfg).await {
                Some(v)
                    if v.get("protocol").and_then(|p| p.as_str())
                        == Some(doorbell_rt::api::PING.protocol) =>
                {
                    eprintln!(
                        "doorbell serve: healthy doorbell peer on {}, exiting 0",
                        cfg.port
                    );
                    std::process::exit(0);
                }
                _ => anyhow::bail!(
                    "doorbell serve: port {} held by non-doorbell squatter",
                    cfg.port
                ),
            }
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
    // Host loops: one Brain per configured workspace. Shutdown = drop
    // every sender (tickers + loop handle), so each brain drains its
    // sinks, journals undelivered intents, and returns its report.
    let loops: Shared<Vec<tokio::task::JoinHandle<doorbell_rt::RunReport>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    for (project, workspace) in &cfg.hosts {
        spawn_host_loop(cfg, &hosted, &loops, project, workspace);
    }
    let rotate_tick = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            copy_truncate_over(&log_path, LOGROTATE_BYTES);
        }
    });
    let app = AppState {
        started,
        fw_state: cfg.flywheel_state.clone(),
        hosted: hosted.clone(),
        health: std::sync::Arc::new(parking_lot::Mutex::new(CachedHealth {
            body: String::new(),
            computed_at: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(3600))
                .unwrap_or(std::time::Instant::now()),
        })),
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
                tokio::task::spawn_blocking(move || refresh_health_snapshot(&app)).await.ok();
            }
        })
    };
    let _ = health_tick;
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
        async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                reconcile_once(&cfg, &hosted, &loops, &owned);
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
    tokio::select! {
        r = serve_tcp => { finish(); r }
        r = serve_uds => { finish(); r }
        _ = shutdown => {
            // Shutdown travels as a message (ticker + forwarder senders
            // stay open until their tasks end with the brain).
            eprintln!("doorbell serve: shutdown signal received, draining loops");
            let retiring = std::mem::take(&mut *hosted.lock());
            for h in &retiring {
                let _ = h.tx.send(doorbell_rt::BrainMsg::Shutdown).await;
            }
            eprintln!("doorbell serve: shutdown sent, joining loops");
            drop(retiring);
            let handles = std::mem::take(&mut *loops.lock());
            for h in handles {
                let _ = h.await;
            }
            eprintln!("doorbell serve: loops joined, releasing leases");
            finish();
            eprintln!("doorbell serve: shutting down");
            Ok(())
        }
    }
}

/// Spawn one hosted loop (bd-2o1v: shared by boot-time `--host` config and
/// dynamic auto-host) and record it on the shared tables.
fn spawn_host_loop(
    cfg: &Config,
    hosted: &Hosts,
    loops: &Shared<Vec<tokio::task::JoinHandle<doorbell_rt::RunReport>>>,
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
    loops.lock().push(tokio::spawn(async move {
        host_workspace(fw, p, w, loop_tx, rx).await
    }));
    // The table keeps its OWN sender: the loop's clones live inside the task,
    // so this handle is the only one reconcile can address. `try_send` on it
    // is how a retiring loop is told to drain (bd-mk60).
    hosted.lock().push(HostedLoop { key, tx });
}
/// auto-host of every nameable subscribed (project, workspace).
fn reconcile_once(
    cfg: &Config,
    hosted: &Hosts,
    loops: &Shared<Vec<tokio::task::JoinHandle<doorbell_rt::RunReport>>>,
    owned: &Shared<Vec<(String, doorbell_rt::lease::OwnerLease)>>,
) {
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
                spawn_host_loop(cfg, hosted, loops, &project, &workspace);
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
    retire_unsubscribed(cfg, hosted, owned, &subs, &hosted_now);
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

fn parent_of(p: &std::path::Path) -> &std::path::Path {
    p.parent().unwrap_or(std::path::Path::new("/tmp"))
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
    let fresh = app
        .health
        .lock()
        .computed_at
        .elapsed()
        .as_secs()
        < HEALTH_CACHE_TTL_SECS;
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
/// most recent), read back from the watch heartbeat.
fn hosted_watch_age(fw_state: &std::path::Path, hosted: &[String]) -> Option<u64> {
    hosted
        .iter()
        .filter_map(|h| hosted_workspace(h))
        .filter_map(|ws| doorbell_rt::watchbeat::age_secs(fw_state, ws))
        .min()
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

fn health_body(app: &AppState) -> String {
    // bd-2o1v: hosting is dynamic — snapshot under the lock once.
    let hosted = hosted_keys(&app.hosted.lock());
    // D4.4: a torn journal tail degrades loudly here (bd-asr2.4).
    let store = doorbell_rt::FileStore::new(&app.fw_state);
    let mut degraded = Vec::new();
    if store.any_torn_tail() {
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
    let wake_blind = mode == "standby" && !known_workspaces.is_empty();
    if wake_blind {
        degraded.push("standby_with_known_workspaces".to_string());
    }
    let (sweep_age, max_seq) = hosted_sweep_age(&app.fw_state, &hosted);
    let watch_age = hosted_watch_age(&app.fw_state, &hosted);
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
    let restarts = doorbell_rt::tailprobe::count_matches_within_across(
        journals.iter().map(std::path::PathBuf::as_path),
        "RestartWatch",
        now,
        WATCH_WINDOW_SECS,
    );
    if restarts > WATCH_RESTARTS_ALLOWED_PER_WS * ws_names.len() {
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
        // last_event_age_s is MEASURED from the watch heartbeat (None =
        // this workspace has never seen a bus event: wrong bus or dead
        // child), not the literal `null` it used to be.
        "watch": {
            "state": if hosted.is_empty() { "standby" } else { "hosted" },
            "last_event_age_s": watch_age,
        },
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
        "workspaces": workspaces,
        "journal_rows": journal_rows,
        "torn_tail": store.any_torn_tail(),
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
    use super::{REAP_AFTER_SECS, inbox_path_down, reap_candidates, unhosted_subscribed};

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
        assert!(
            HEALTH_CACHE_TTL_SECS <= 30,
            "a health snapshot older than a sweep era lies: {HEALTH_CACHE_TTL_SECS}"
        );
    }
}
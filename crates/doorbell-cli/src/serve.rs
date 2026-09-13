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
    let toron_bin = std::env::var("DOORBELL_TORON_BIN").unwrap_or_else(|_| "toron".into());
    let herdr_bin = std::env::var("DOORBELL_HERDR_BIN").unwrap_or_else(|_| "herdr".into());
    let mut fcfg = FileHostConfig::new(fw_state.clone(), project.clone(), workspace.clone());
    fcfg.toron_bin = toron_bin;
    fcfg.herdr_bin = herdr_bin.clone();
    let mut host = FileHost::new(fcfg).with_tx(tx.clone());
    // Daemon-keyed generation: first run captures the doorbell binary
    // identity so a mid-run upgrade retires the loop instead of waking
    // stale (same rule as the embedded WatcherGen gate, daemon-keyed).
    doorbell_rt::ensure_daemon_gen(&fw_state, &workspace);
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
    /// bd-2o1v: hosting is dynamic (auto-host on subscribe), so the list
    /// is shared mutably; handlers snapshot it under the lock.
    hosted: Shared<Vec<String>>,
}

/// bd-2o1v: shared mutable state handle (parking_lot — no poisoning).
type Shared<T> = std::sync::Arc<parking_lot::Mutex<T>>;

/// Reap threshold for orphaned spawn-scoped subscriptions (bd-2o1v): a
/// crashed fleet's rows die after a week of journal silence. `kill`
/// normally drops its own rows; this is the belt.
const REAP_AFTER_SECS: u64 = 7 * 24 * 3600;

/// (project, workspace) pairs named by subscriptions but not yet hosted.
/// Legacy rows without a project are never guessed into a host.
fn unhosted_subscribed(
    subs: &[doorbell_rt::subscribe::Subscription],
    hosted: &[String],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for sub in subs {
        let Some(project) = &sub.project else { continue };
        for ws in sub.scope.workspaces() {
            let key = format!("{project}/{ws}");
            if !hosted.contains(&key) && !out.contains(&(project.clone(), ws.clone())) {
                out.push((project.clone(), ws));
            }
        }
    }
    out
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
    // bd-2o1v: `hosted` starts EMPTY — `spawn_host_loop` is the sole writer,
    // so the list names only loops that actually exist (a pre-populated
    // list made the idempotency guard skip every boot host: health showed
    // hosted strings with no Brain behind them, zero sweeps, clean exit).
    let hosted: Shared<Vec<String>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
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
    let loop_txs: Shared<Vec<tokio::sync::mpsc::Sender<doorbell_rt::BrainMsg>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    for (project, workspace) in &cfg.hosts {
        spawn_host_loop(cfg, &hosted, &loops, &loop_txs, project, workspace);
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
    };
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
        let loop_txs = loop_txs.clone();
        let owned = owned.clone();
        async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                reconcile_once(&cfg, &hosted, &loops, &loop_txs, &owned);
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
            let txs = std::mem::take(&mut *loop_txs.lock());
            for tx in &txs {
                let _ = tx.send(doorbell_rt::BrainMsg::Shutdown).await;
            }
            eprintln!("doorbell serve: shutdown sent, joining loops");
            drop(txs);
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
    hosted: &Shared<Vec<String>>,
    loops: &Shared<Vec<tokio::task::JoinHandle<doorbell_rt::RunReport>>>,
    loop_txs: &Shared<Vec<tokio::sync::mpsc::Sender<doorbell_rt::BrainMsg>>>,
    project: &str,
    workspace: &str,
) {
    // Idempotent: two callers (boot config + reconcile auto-host) may race
    // the same pair — a second Brain on one workspace double-rings. Check
    // before pushing anything, so an early return leaves no orphan tx.
    let key = format!("{project}/{workspace}");
    if hosted.lock().contains(&key) {
        return;
    }
    let (tx, rx) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
    loop_txs.lock().push(tx.clone());
    let (fw, p, w) = (
        cfg.flywheel_state.clone(),
        project.to_string(),
        workspace.to_string(),
    );
    loops.lock().push(tokio::spawn(async move {
        host_workspace(fw, p, w, tx, rx).await
    }));
    hosted.lock().push(key);
}
/// auto-host of every nameable subscribed (project, workspace).
fn reconcile_once(
    cfg: &Config,
    hosted: &Shared<Vec<String>>,
    loops: &Shared<Vec<tokio::task::JoinHandle<doorbell_rt::RunReport>>>,
    loop_txs: &Shared<Vec<tokio::sync::mpsc::Sender<doorbell_rt::BrainMsg>>>,
    owned: &Shared<Vec<(String, doorbell_rt::lease::OwnerLease)>>,
) {
    use doorbell_rt::lease;
    use doorbell_rt::subscribe::{Actor, SubscriptionStore};
    let mut store = SubscriptionStore::open(&cfg.flywheel_state);
    let subs = store.list();
    for id in reap_candidates(&subs, |ws| journal_age_secs(&cfg.flywheel_state, ws)) {
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
    let unhosted = unhosted_subscribed(&store.list(), &hosted.lock().clone());
    for (project, workspace) in unhosted {
        if let Some(cur) = lease::read_lease(&cfg.flywheel_state, &workspace)
            && !lease::lease_stale(&cur, now)
        {
            continue; // a live writer (embedded loop) owns it — never steal
        }
        match lease::acquire_lease(&cfg.flywheel_state, &workspace, now) {
            Ok(l) => {
                owned.lock().push((workspace.clone(), l));
                eprintln!(
                    "doorbell serve: auto-hosting {project}/{workspace} (subscribed)"
                );
                spawn_host_loop(cfg, hosted, loops, loop_txs, &project, &workspace);
            }
            Err(e) => eprintln!(
                "doorbell serve: auto-host {project}/{workspace} lease failed: {e:?}"
            ),
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
        "/health" => ("200 OK", "application/json", health_body(&app)),
        "/stats" => ("200 OK", "application/json", stats_body(&app)),
        "/events" => {
            let since = path
                .split("since=")
                .nth(1)
                .and_then(|s| s.split('&').next()?.parse::<u64>().ok())
                .or_else(|| {
                    req.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        (k.trim().to_ascii_lowercase() == "last-event-id")
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
    let hosted = app.hosted.lock().clone();
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
    let hosted = app.hosted.lock().clone();
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
                journal_rows += store.journal_row_count(&rest.to_string());
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
    use super::{reap_candidates, unhosted_subscribed, REAP_AFTER_SECS};

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
}

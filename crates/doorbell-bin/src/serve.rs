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
            since: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
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
    hosted: Vec<String>,
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
    let mut owned: Vec<(String, doorbell_rt::lease::OwnerLease)> = Vec::new();
    for (_, workspace) in &cfg.hosts {
        match lease::acquire_lease(&cfg.flywheel_state, workspace, lease::unix_now()) {
            Ok(l) => owned.push((workspace.clone(), l)),
            Err(e) => {
                for (ws, l) in &owned {
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
    let hosted: Vec<String> = cfg.hosts.iter().map(|(p, w)| format!("{p}/{w}")).collect();
    eprintln!(
        "doorbell serve: up on 127.0.0.1:{} + {} (hosting {})",
        cfg.port,
        sock_path.display(),
        if hosted.is_empty() {
            "nothing (standby)".to_string()
        } else {
            hosted.join(", ")
        }
    );
    // Host loops: one Brain per configured workspace. Shutdown = drop
    // every sender (tickers + loop handle), so each brain drains its
    // sinks, journals undelivered intents, and returns its report.
    let mut loops = Vec::new();
    let mut loop_txs = Vec::new();
    for (project, workspace) in &cfg.hosts {
        let (tx, rx) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        loop_txs.push(tx.clone());
        let (fw, p, w) = (
            cfg.flywheel_state.clone(),
            project.clone(),
            workspace.clone(),
        );
        loops.push(tokio::spawn(async move {
            host_workspace(fw, p, w, tx, rx).await
        }));
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
    // D9 layer 1 upkeep: heartbeat every 30s per held lease.
    let hb_state = cfg.flywheel_state.clone();
    let hb_owned: Vec<String> = owned.iter().map(|(ws, _)| ws.clone()).collect();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            let now = doorbell_rt::lease::unix_now();
            for ws in &hb_owned {
                if let Some(mut cur) = doorbell_rt::lease::read_lease(&hb_state, ws) {
                    doorbell_rt::lease::refresh_lease(&hb_state, ws, &mut cur, now);
                }
            }
        }
    });
    let finish = |owned: &[(String, doorbell_rt::lease::OwnerLease)]| {
        heartbeat.abort();
        rotate_tick.abort();
        crate::lifecycle::release_pid_file(&cfg.pid_file(), std::process::id());
        for (ws, l) in owned {
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
        r = serve_tcp => { finish(&owned); r }
        r = serve_uds => { finish(&owned); r }
        _ = shutdown => {
            // Shutdown travels as a message (ticker + forwarder senders
            // stay open until their tasks end with the brain).
            eprintln!("doorbell serve: shutdown signal received, draining loops");
            for tx in &loop_txs {
                let _ = tx.send(doorbell_rt::BrainMsg::Shutdown).await;
            }
            eprintln!("doorbell serve: shutdown sent, joining loops");
            drop(loop_txs);
            for h in loops {
                let _ = h.await;
            }
            eprintln!("doorbell serve: loops joined, releasing leases");
            finish(&owned);
            eprintln!("doorbell serve: shutting down");
            Ok(())
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
fn hosted_sweep_age(fw_state: &std::path::Path, hosted: &[String]) -> (Option<u64>, u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut min_age: Option<u64> = None;
    let mut max_seq = 0u64;
    for hw in hosted {
        let Some(ws) = hw.split('/').nth(1) else {
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
fn health_body(app: &AppState) -> String {
    // D4.4: a torn journal tail degrades loudly here (bd-asr2.4).
    let store = doorbell_rt::FileStore::new(&app.fw_state);
    let mut degraded = Vec::new();
    if store.any_torn_tail() {
        degraded.push("journal_torn_tail".to_string());
    }
    let mode = if app.hosted.is_empty() {
        "standby"
    } else {
        "hosted"
    };
    let (sweep_age, max_seq) = hosted_sweep_age(&app.fw_state, &app.hosted);
    serde_json::json!({
        "protocol": doorbell_rt::api::PING.protocol,
        "core_v": doorbell_rt::api::PING.core_v,
        "journal_v": doorbell_rt::api::PING.journal_v,
        "uptime_s": app.started.elapsed().as_secs(),
        "mode": mode,
        "hosted": app.hosted,
        "watch": {"state": if app.hosted.is_empty() { "standby" } else { "hosted" }, "last_event_age_s": null},
        "sweep": {"last_age_s": sweep_age},
        "subscribers": 0,
        "degraded": degraded,
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
        "mode": if app.hosted.is_empty() { "standby" } else { "hosted" },
        "hosted": app.hosted,
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

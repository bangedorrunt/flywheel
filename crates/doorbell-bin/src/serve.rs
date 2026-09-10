// governed-by: ADR-0012
//! Serve loop (D2): UDS + TCP bind, hand-rolled `/health`, hourly 1 MiB
//! copy-truncate logrotate. No HTTP framework: one GET route needs
//! sixty lines, not a dependency.
//!
//! Standby in Phase 1: no brain is hosted yet (tick binding is bd-asr2.5),
//! so liveness fields report honestly and dynamic ones wait their phase.

use crate::lifecycle::Config;
use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LOGROTATE_BYTES: u64 = 1024 * 1024;

pub async fn run(cfg: &Config) -> Result<()> {
    use doorbell_rt::lease;
    let now = lease::unix_now();
    // D9 layer 1 first: a live lease means another writer runs — refuse
    // before touching its socket (never clobber a stranger's bind).
    if let Some(cur) = lease::read_lease(&cfg.state_dir)
        && !lease::lease_stale(&cur, now)
    {
        anyhow::bail!(
            "doorbell serve: refused, live lease held by pid {} on {} (started {})",
            cur.pid,
            cur.host,
            cur.started_unix
        );
    }
    let tcp = match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", cfg.port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // D9 layer 2: healthy doorbell peer → clean exit 0 (no respawn
            // war under Restart=on-failure); anyone else → hard fail naming
            // the squatter (toron serve_lock precedent).
            match peer_health(cfg).await {
                Some(v) if v.get("protocol").and_then(|p| p.as_str()) == Some(doorbell_rt::api::PING.protocol) => {
                    eprintln!("doorbell serve: healthy doorbell peer on {}, exiting 0", cfg.port);
                    std::process::exit(0);
                }
                _ => anyhow::bail!("doorbell serve: port {} held by non-doorbell squatter", cfg.port),
            }
        }
        Err(e) => return Err(e).with_context(|| format!("bind 127.0.0.1:{}", cfg.port)),
    };
    // Claim the writer lease before serving (heartbeat below keeps it).
    let owned = lease::acquire_lease(&cfg.state_dir, lease::unix_now()).map_err(|e| {
        anyhow::anyhow!("doorbell serve: lease claim failed after bind: {e:?}")
    })?;
    let sock_path = cfg.socket_path();
    if let Some(parent) = sock_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent_of(&sock_path), std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_file(&sock_path);
    }
    let uds = tokio::net::UnixListener::bind(&sock_path)
        .with_context(|| format!("bind {}", sock_path.display()))?;
    let started = std::time::Instant::now();
    let log_path = cfg.log_file();
    eprintln!("doorbell serve: up on 127.0.0.1:{} + {}", cfg.port, sock_path.display());
    let rotate_tick = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            copy_truncate_over(&log_path, LOGROTATE_BYTES);
        }
    });
    let serve_tcp = async {
        loop {
            let (stream, _) = tcp.accept().await?;
            tokio::spawn(handle_http(stream, started));
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let serve_uds = async {
        loop {
            let (stream, _) = uds.accept().await?;
            tokio::spawn(handle_uds(stream, started));
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    // D9 layer 1 upkeep: heartbeat every 30s (the live dispatcher rewrites
    // it each sweep in .5; the daemon has no sweep yet, so wall-clock it).
    let hb_state = cfg.state_dir.clone();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            let now = doorbell_rt::lease::unix_now();
            if let Some(mut cur) = doorbell_rt::lease::read_lease(&hb_state) {
                doorbell_rt::lease::refresh_lease(&hb_state, &mut cur, now);
            }
        }
    });
    let finish = |owned: &doorbell_rt::lease::OwnerLease| {
        heartbeat.abort();
        rotate_tick.abort();
        doorbell_rt::lease::release_lease(&cfg.state_dir, owned);
    };
    tokio::select! {
        r = serve_tcp => { finish(&owned); r }
        r = serve_uds => { finish(&owned); r }
        _ = tokio::signal::ctrl_c() => {
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
            stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
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

async fn handle_http(mut stream: tokio::net::TcpStream, started: std::time::Instant) {
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
    let (code, body) = match path {
        "/health" => ("200 OK", health_body(started)),
        _ => ("404 Not Found", serde_json::json!({"error": "not found"}).to_string()),
    };
    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
}

async fn handle_uds(mut stream: tokio::net::UnixStream, started: std::time::Instant) {
    // UDS speaks the same minimal surface: one JSON line in, one out.
    // Verbs ride here in Phase 2; today only `ping` answers.
    let mut buf = vec![0u8; 4096];
    let Ok(n) = stream.read(&mut buf).await else {
        return;
    };
    let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    let body = if req.contains("ping") {
        let p = &doorbell_rt::api::PING;
        serde_json::json!({"protocol": p.protocol, "core_v": p.core_v, "journal_v": p.journal_v}).to_string() + "\n"
    } else {
        serde_json::json!({"alive": true, "uptime_s": started.elapsed().as_secs()}).to_string()
            + "\n"
    };
    let _ = stream.write_all(body.as_bytes()).await;
}

/// D2 `/health`: read-only, unauthenticated on loopback/UDS. Dynamic
/// fields report standby until their phase binds them (subscribers:
/// bd-asr2.8; watch/sweep: bd-asr2.5; journal seq: bd-asr2.6).
fn health_body(started: std::time::Instant) -> String {
    serde_json::json!({
        "protocol": doorbell_rt::api::PING.protocol,
        "core_v": doorbell_rt::api::PING.core_v,
        "journal_v": doorbell_rt::api::PING.journal_v,
        "uptime_s": started.elapsed().as_secs(),
        "watch": {"state": "standby", "last_event_age_s": null},
        "sweep": {"last_age_s": null},
        "subscribers": 0,
        "degraded": [],
        "journal": {"seq": 0, "lag_lines": 0},
        "owner": {"pid": std::process::id(), "gen": null, "host": hostname()},
    })
    .to_string()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

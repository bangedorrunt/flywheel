// governed-by: ADR-0012
//! Version handshake verb (D8/E16, bd-asr2.12): fetch the daemon's
//! versions, compare against the CLI's (or `--expect-*` pins), and on skew
//! name BOTH versions instead of mis-rendering.

use crate::lifecycle::Config;
use anyhow::{Context, Result};

pub fn handshake(
    cfg: &Config,
    expect_core: Option<String>,
    expect_journal: Option<u64>,
) -> Result<()> {
    let body = health_body(cfg.port)?;
    let v: serde_json::Value = serde_json::from_str(&body).context("parse /health")?;
    let daemon_core = v
        .get("core_v")
        .and_then(|c| c.as_str())
        .unwrap_or("unknown");
    let daemon_journal = v.get("journal_v").and_then(|j| j.as_u64()).unwrap_or(0);
    let want_core = expect_core
        .as_deref()
        .unwrap_or(doorbell_rt::api::PING.core_v);
    let want_journal = expect_journal.unwrap_or(doorbell_rt::api::PING.journal_v);
    println!(
        "{}",
        serde_json::json!({
            "protocol": doorbell_rt::api::PING.protocol,
            "daemon": {"core_v": daemon_core, "journal_v": daemon_journal},
            "cli": {"core_v": want_core, "journal_v": want_journal},
        })
    );
    doorbell_rt::api::check_compat_with(want_core, want_journal, daemon_core, daemon_journal)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn health_body(port: u16) -> Result<String> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse()?,
        std::time::Duration::from_secs(3),
    )
    .context("connect daemon")?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )?;
    let mut body = String::new();
    stream.read_to_string(&mut body)?;
    let idx = body.find("\r\n\r\n").context("split headers")?;
    Ok(body[idx + 4..].to_string())
}

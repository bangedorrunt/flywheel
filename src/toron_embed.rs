//! In-process toron integration surface (ADR-0003 Decision 3A, bd-o8j.1).
//!
//! flywheel embeds toron in-process — zero IPC. The mailbox is constructed via
//! [`toron_mcp::demo_mailbox`] (an `InProcessBus` + SQLite index mailbox in the
//! system temp dir), so every subcommand below exercises the real toron-core
//! mailbox over an in-memory bus and prints the exact `ToJson` shapes the MCP
//! server emits (JSON parity with the toron robot CLI).
//!
//! `start-mcp` additionally proves the MCP endpoint runs in the SAME process as
//! flywheel: the Streamable HTTP / stdio server is spawned on a tokio task and
//! stays up until Ctrl-C.
//!
//! governed-by: ADR-0003

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::json;

use toron_core::mailbox::{FetchInbox, SendMessage};
use toron_mcp::ToJson;

use crate::config::Config;
use crate::output::print_json;
use crate::{ToronCmd, ToronMailCmd};

/// Build the in-process demo mailbox exactly as toron-mcp does.
async fn mailbox() -> Result<toron_core::mailbox::Mailbox> {
    toron_mcp::demo_mailbox().await
}

/// `flywheel toron <subcommand>` dispatch (block_on'd from a tokio runtime by
/// `cmd_toron` in main.rs). `_cfg` is unused: the demo mailbox is fully
/// in-process, nothing reads flywheel config.
pub async fn dispatch(_cfg: &Config, cmd: &ToronCmd) -> Result<()> {
    match cmd {
        ToronCmd::Status => {
            let mb = mailbox().await?;
            let report = mb.health_check().await;
            print_json(&report.to_json());
        }
        ToronCmd::Mail { cmd } => cmd_mail(cmd).await?,
        ToronCmd::Agents { project } => {
            let mb = mailbox().await?;
            let agents = mb
                .list_agents(project, toron_core::mailbox::ListAgentsOptions::default())
                .await?;
            print_json(&agents.to_json());
        }
        ToronCmd::StartMcp { port, stdio } => start_mcp(*port, *stdio).await?,
        // SetupMcp is handled synchronously in cmd_toron (pure file I/O, no
        // embed runtime needed) before this dispatch is reached. Reaching here
        // is a logic error — fail loudly rather than silently no-op.
        ToronCmd::SetupMcp { .. } => {
            return Err(anyhow::anyhow!(
                "toron setup-mcp must run outside the in-process embed dispatch"
            ))
        }
    }
    Ok(())
}

async fn cmd_mail(cmd: &ToronMailCmd) -> Result<()> {
    match cmd {
        ToronMailCmd::Send {
            project,
            from,
            to,
            subject,
            body,
        } => {
            let mb = mailbox().await?;
            let recipients: Vec<String> = to
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if recipients.is_empty() {
                bail!("mail send: no recipients (--to must be a non-empty comma-separated list)");
            }
            let result = mb
                .send_message(&SendMessage {
                    project_slug: project.clone(),
                    sender_name: from.clone(),
                    to: recipients,
                    subject: subject.clone(),
                    body_md: body.clone(),
                    ..Default::default()
                })
                .await?;
            print_json(&result.to_json());
        }
        ToronMailCmd::Inbox {
            project,
            agent,
            limit,
        } => {
            let mb = mailbox().await?;
            let result = mb
                .fetch_inbox(&FetchInbox {
                    project_slug: project.clone(),
                    agent_name: agent.clone(),
                    limit: *limit,
                    ..Default::default()
                })
                .await?;
            print_json(&result.to_json());
        }
    }
    Ok(())
}

/// Serve the toron MCP endpoint from this process on a background tokio task,
/// then block until Ctrl-C (or until the server exits on its own).
async fn start_mcp(port: u16, stdio: bool) -> Result<()> {
    let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let handle = tokio::task::spawn(async move {
        let res = if stdio {
            toron_mcp::serve_stdio().await
        } else {
            toron_mcp::serve_http(&format!("127.0.0.1:{port}")).await
        };
        let _ = tx.send(res);
    });

    // Brief pause so a bind failure surfaces through `rx` instead of the
    // process hanging on Ctrl-C.
    tokio::time::sleep(Duration::from_millis(300)).await;
    match rx.try_recv() {
        Ok(res) => {
            handle.abort();
            return res;
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            handle.abort();
            bail!("toron MCP server task died before serving");
        }
    }
    if stdio {
        print_json(&json!({
            "status": "serving",
            "transport": "stdio",
            "pid": std::process::id(),
        }));
    } else {
        print_json(&json!({
            "status": "serving",
            "transport": "streamable-http",
            "addr": format!("127.0.0.1:{port}"),
            "endpoint": "/mcp",
            "pid": std::process::id(),
        }));
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            handle.abort();
            print_json(&json!({ "status": "stopped" }));
            Ok(())
        }
        res = rx => {
            handle.abort();
            match res {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(_) => Ok(()),
            }
        }
    }
}

//! `flywheel toron` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::Result;
use clap::Subcommand;

use crate::config::Config;

#[derive(Subcommand)]
pub(crate) enum ToronCmd {
    /// In-process toron mailbox health (bus + archive) as JSON
    Status,
    /// Exercise the in-process toron mailbox message surface (zero IPC)
    Mail {
        #[command(subcommand)]
        cmd: ToronMailCmd,
    },
    /// List registered agents in a project as JSON (empty array on a fresh
    /// demo mailbox — the temp SQLite bus is per-process)
    Agents {
        /// project slug
        #[arg(long)]
        project: String,
    },
    /// Serve the toron MCP endpoint from this process on a background task;
    /// blocks until Ctrl-C
    #[command(name = "start-mcp")]
    StartMcp {
        /// Streamable HTTP bind port on 127.0.0.1 (default 8000)
        #[arg(long, default_value_t = 8000)]
        port: u16,
        /// serve over stdio instead of Streamable HTTP
        #[arg(long)]
        stdio: bool,
    },
    /// Auto-wire the toron MCP server into the agent MCP configs
    /// (opencode ~/.config/opencode/opencode.json, pi ~/.pi/agent/mcp.json,
    /// grok ~/.grok/config.toml). Idempotent — re-running only updates the
    /// toron entry, never clobbers other servers.
    #[command(name = "setup-mcp")]
    SetupMcp {
        /// toron MCP server URL (default http://127.0.0.1:18080/mcp)
        #[arg(long)]
        url: Option<String>,
        /// which agents to wire: opencode,pi,grok (comma-sep; default all)
        #[arg(long, default_value = "opencode,pi,grok")]
        agents: String,
        /// also boot a toron serve --mcp-http <port> daemon (needs the toron
        /// binary on PATH and TORON_OPERATOR_KEY / TORON_KEY_PASSPHRASE set)
        #[arg(long, default_value_t = 0)]
        boot_port: u16,
        /// print what would change without touching any file
        #[arg(long)]
        dry_run: bool,
        /// also point flywheel's OWN mail backend at the same toron URL
        /// (writes `[mail.toron] url` in the flywheel config.toml, so flywheel
        /// + all agents share one mailbox)
        #[arg(long)]
        flywheel: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ToronMailCmd {
    /// send a message via the in-process mailbox
    Send {
        #[arg(long)]
        project: String,
        #[arg(long)]
        from: String,
        /// comma-separated recipient names
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
    },
    /// fetch an agent's inbox from the in-process mailbox
    Inbox {
        #[arg(long)]
        project: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        limit: Option<usize>,
    },
}

pub(crate) fn cmd_toron(cfg: &Config, cmd: &ToronCmd) -> Result<()> {
    // The MCP-config wiring is pure file I/O — it runs synchronously and does
    // not need the tokio embed runtime. Handle it before delegating to the
    // in-process mailbox dispatch.
    if let ToronCmd::SetupMcp {
        url,
        agents,
        boot_port,
        dry_run,
        flywheel,
    } = cmd
    {
        return crate::toron_setup_mcp(url.as_deref(), agents, *boot_port, *dry_run, *flywheel);
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(crate::toron_embed::dispatch(cfg, cmd))
}

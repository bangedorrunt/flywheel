// governed-by: ADR-0012
//! `doorbell` daemon binary (ADR-0012 D2, Phase 1).
//!
//! Toron-parity lifecycle (corrected per the rev-2 audit): UDS primary +
//! TCP `127.0.0.1:18980`, JSON `/health` (toron has none — doorbell's
//! consumers are non-MCP), `Restart=on-failure` units, pidfile fallback,
//! verify-before-exit-0, hourly 1 MiB copy-truncate logrotate from day one.
//!
//! Phase 1 serves `/health` in standby (no brain yet — the tick-host
//! binding lands in bd-asr2.5); every standby field says so explicitly.

mod lifecycle;
mod serve;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "doorbell", about = "doorbell plane-D daemon (ADR-0012)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// State dir override (default: platform data dir / doorbell).
    #[arg(long, global = true)]
    state_dir: Option<std::path::PathBuf>,
    /// TCP port override, mostly for tests (default 18980).
    #[arg(long, global = true)]
    port: Option<u16>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the daemon (bind UDS+TCP, verify /health, exit 0).
    Start,
    /// Stop the daemon (5s grace; refuses unmanaged holders w/o --force).
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// Stop, start, re-verify.
    Restart,
    /// JSON status (no side effects).
    Status,
    /// Run the serve loop (spawned by `start`; humans rarely run this).
    Serve,
    /// Daemon command (compat alias for serve foreground).
    #[command(hide = true)]
    InternalServe,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = lifecycle::Config::resolve(cli.state_dir, cli.port)?;
    match cli.cmd {
        Cmd::Start => lifecycle::start(&cfg),
        Cmd::Stop { force } => lifecycle::stop(&cfg, force),
        Cmd::Restart => lifecycle::restart(&cfg),
        Cmd::Status => lifecycle::status(&cfg),
        Cmd::Serve | Cmd::InternalServe => serve::run(&cfg).await,
    }
}

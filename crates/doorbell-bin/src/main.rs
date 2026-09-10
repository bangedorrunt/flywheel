// governed-by: ADR-0012
//! `doorbell` daemon binary (ADR-0012 D2, Phase 1).

mod lifecycle;
mod migrate;
mod ping;
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
    /// Adopt a dispatcher's state dir byte-identically (bd-asr2.6).
    Migrate {
        /// Source state dir to adopt from.
        #[arg(long)]
        from: std::path::PathBuf,
    },
    /// Version handshake against the daemon (E16): names both versions on
    /// skew instead of mis-rendering. `--expect-*` pins the CLI side.
    Ping {
        /// Expected core version (default: this binary's).
        #[arg(long)]
        expect_core: Option<String>,
        /// Expected journal version (default: this binary's).
        #[arg(long)]
        expect_journal: Option<u64>,
    },
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
        Cmd::Migrate { from } => {
            let copied = migrate::migrate(&from, &cfg.state_dir)?;
            println!("{}", serde_json::json!({"migrated": copied}).to_string());
            Ok(())
        }
        Cmd::Ping {
            expect_core,
            expect_journal,
        } => ping::handshake(&cfg, expect_core, expect_journal),
    }
}

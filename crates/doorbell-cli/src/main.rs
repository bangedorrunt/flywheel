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
    /// State dir override (default: $DOORBELL_STATE_DIR, else
    /// $XDG_STATE_HOME/doorbell, else ~/.local/state/doorbell).
    #[arg(long, global = true)]
    state_dir: Option<std::path::PathBuf>,
    /// TCP port override, mostly for tests (default 18980).
    #[arg(long, global = true)]
    port: Option<u16>,
    /// Flywheel state dir override (journals, leases, registries).
    /// Default mirrors flywheel: $XDG_STATE_HOME/flywheel, else platform.
    #[arg(long, global = true)]
    flywheel_state_dir: Option<std::path::PathBuf>,
    /// Hosted loop as project/workspace, repeatable (bd-ene2 Phase 3).
    /// Empty = standby: bind + reads only.
    #[arg(long, global = true)]
    host: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the daemon (bind UDS+TCP, verify /health, exit 0).
    Start,
    /// Stop the daemon (TERM + 15s drain grace, then KILL; endpoint-verified;
    /// refuses unmanaged holders w/o --force).
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// Stop, start, re-verify.
    Restart,
    /// JSON status (no side effects).
    Status,
    /// Install the autostart unit (idempotent) and load it: macOS
    /// launchd agent, Linux systemd user unit. Afterwards the daemon
    /// survives reboots; `start` keeps working for right-now.
    Install,
    /// Run the serve loop (spawned by `start`; humans rarely run this).
    Serve,
    /// Adopt a dispatcher's state dir byte-identically (bd-asr2.6).
    /// Also runs automatically once at startup when the default state dir
    /// is in effect and the legacy platform dir still holds content.
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
    let mut hosts = Vec::new();
    for h in &cli.host {
        hosts.push(lifecycle::parse_host(h)?);
    }
    let using_default_dir =
        cli.state_dir.is_none() && lifecycle::env_is_unset_or_empty("DOORBELL_STATE_DIR");
    let cfg =
        lifecycle::Config::resolve_full(cli.state_dir, cli.port, cli.flywheel_state_dir, hosts)?;
    // bd-fpd5: migrate-once off the legacy platform dir into the XDG
    // default. Explicit --state-dir / DOORBELL_STATE_DIR means
    // user-managed: never touch, migrate manually instead.
    if using_default_dir && let Some(old) = lifecycle::legacy_platform_state_dir() {
        match lifecycle::maybe_migrate_to_default(&cfg.state_dir, &old) {
            // Silent when nothing actually moved: an EMPTY legacy dir would
            // otherwise re-print this line on every doorbell command.
            Ok(Some(copied)) if !copied.is_empty() => eprintln!(
                "doorbell: migrated {} file(s) from {} (one-time XDG move)",
                copied.len(),
                old.display()
            ),
            Ok(_) => {}
            Err(e) => anyhow::bail!("doorbell: state migration failed: {e:#}"),
        }
    }
    match cli.cmd {
        Cmd::Start => lifecycle::start(&cfg),
        Cmd::Stop { force } => lifecycle::stop(&cfg, force),
        Cmd::Restart => lifecycle::restart(&cfg),
        Cmd::Status => lifecycle::status(&cfg),
        Cmd::Install => lifecycle::install(&cfg),
        Cmd::Serve | Cmd::InternalServe => {
            // A daemon must exit the moment its shutdown completes: the
            // tokio runtime drop waits on in-flight blocking tasks
            // (health refresh, CLI probes), which kept this process alive
            // past stop's 15s grace AFTER "shutting down" was already
            // printed — the false SIGKILL escalation of 16:48. finish()
            // already released the pidfile and leases, so nothing else
            // needs to run; an Err still exits non-zero.
            match serve::run(&cfg).await {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Migrate { from } => {
            let copied = migrate::migrate(&from, &cfg.state_dir)?;
            println!("{}", serde_json::json!({"migrated": copied}));
            Ok(())
        }
        Cmd::Ping {
            expect_core,
            expect_journal,
        } => ping::handshake(&cfg, expect_core, expect_journal),
    }
}

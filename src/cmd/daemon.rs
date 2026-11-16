//! `flywheel daemon` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[cfg(feature = "serve")]
/// Daemon lifecycle subcommands (toron-parity, `src/daemon.rs`).
#[derive(Subcommand)]
pub(crate) enum DaemonCmd {
    /// Ensure the daemon is running (install-if-absent; pidfile fallback).
    Start {
        /// serve port (default `daemon::SERVE_PORT_DEFAULT`)
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
        /// Bounce live daemons as the operator: bypass the captain-only guard.
        #[arg(long)]
        operator: bool,
    },
    /// Stop the daemons (serve + cron). Idempotent; refuses unmanaged holders
    /// without --force.
    Stop {
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
        /// kill an unmanaged port holder (a stray manual `flywheel serve`)
        #[arg(long)]
        force: bool,
        /// Bounce live daemons as the operator: bypass the captain-only guard.
        #[arg(long)]
        operator: bool,
    },
    /// Stop then start, VERIFYING the new daemon answers before exit 0.
    Restart {
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
        #[arg(long)]
        force: bool,
        /// Bounce live daemons as the operator: bypass the captain-only guard.
        #[arg(long)]
        operator: bool,
    },
    /// Print how the daemon is currently supervised / running.
    Status {
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
    },
}

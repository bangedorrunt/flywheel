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
    },
    /// Stop the daemon. Idempotent; refuses unmanaged holders without --force.
    Stop {
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
        /// kill an unmanaged port holder (a stray manual `flywheel serve`)
        #[arg(long)]
        force: bool,
    },
    /// Stop then start, VERIFYING the new daemon answers before exit 0.
    Restart {
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
        #[arg(long)]
        force: bool,
    },
    /// Print how the daemon is currently supervised / running.
    Status {
        #[arg(long, default_value_t = crate::daemon::SERVE_PORT_DEFAULT)]
        port: u16,
    },
}

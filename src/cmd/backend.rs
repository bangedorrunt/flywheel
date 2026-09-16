//! `flywheel backend` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// `flywheel backend` subcommands (ADR-0002 R0.10) — discovery + selection of
/// the pluggable multiplexer/mail backends. clig.dev-disciplined: stdout=data,
/// --json on reads, exit 0/1/2.
#[derive(Subcommand)]
pub(crate) enum BackendCmd {
    /// List compiled-in backends (name, kind, ready|stub, active, feature)
    List,
    /// Show the active multiplexer + mail + config source + health
    Status,
    /// Persist a backend default to config.toml [backend] (env/flag still override)
    Default {
        /// multiplexer | mail
        kind: String,
        /// backend name (herdr|superlogical|toron)
        name: String,
    },
}

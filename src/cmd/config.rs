//! `flywheel config` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
    List,
    Get { key: String },
    Set { key: String, value: String },
    Path,
}

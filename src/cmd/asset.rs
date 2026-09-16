//! `flywheel asset` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum AssetCmd {
    List,
    Run { name: String, project: String },
}

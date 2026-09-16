//! `flywheel policy` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum PolicyCmd {
    List,
    Add { pattern: String },
    Remove { index: usize },
    Path,
}

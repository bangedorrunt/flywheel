//! `flywheel checkpoint` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum CheckpointCmd {
    Save {
        project: String,
        #[arg(long)]
        msg: String,
    },
    List {
        project: String,
    },
    Show {
        project: String,
    },
    Latest {
        project: String,
    },
}

//! `flywheel worktree` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum WorktreeCmd {
    List {
        project: String,
    },
    Create {
        project: String,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },
    Open {
        project: String,
        #[arg(long)]
        branch: String,
    },
    Remove {
        project: String,
        #[arg(long)]
        force: bool,
    },
}

//! `flywheel artifacts` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum ArtifactsCmd {
    /// List artifacts for a run (reads state-dir artifacts/<run_id>)
    #[command(name = "stitch")]
    Stitch {
        /// run id to stitch
        run_id: String,
        /// project for artifact path (default inferred from current dir)
        #[arg(long)]
        project: Option<String>,
    },
}

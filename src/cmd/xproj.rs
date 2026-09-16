//! `flywheel xproj` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Cross-project swarm fan-out (bd-pattern-cross-project-swarm).
#[derive(Subcommand)]
pub(crate) enum XprojCmd {
    /// one-time contact handshake with an agent in another project
    Request {
        /// local project (sender's workspace path/label)
        project: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        to_project: String,
        #[arg(long, default_value = "cross-project swarm fan-out")]
        reason: String,
    },
    /// send a cross-project swarm request (`[xproj]` subject prefix)
    Send {
        /// local project (sender)
        project: String,
        #[arg(long)]
        to: String,
        /// absolute path or label of the receiving project
        #[arg(long)]
        to_project: String,
        /// recipe/workflow name the receiver should run
        #[arg(long)]
        recipe: String,
        /// free-form params for the receiver (strategy, asset, windows, …)
        #[arg(long, default_value = "")]
        body: String,
        /// optional product for reply search (default: trading-research)
        #[arg(long, default_value = "trading-research")]
        product: String,
        /// worker count hint for the receiver
        #[arg(long, default_value_t = 2)]
        workers: u32,
    },
    /// parse an `[xproj]` body (debug / orchestrator unit path)
    Parse {
        /// full message body or subject+body text
        text: String,
    },
}

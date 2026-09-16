//! `flywheel runs` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Runs view (ADR-0005 P6.3): the captain's daily dashboard over workflow /
/// pipeline / loop runs recorded in the project log.
#[derive(Subcommand)]
pub(crate) enum RunsCmd {
    /// List runs across projects: state + owner + per-run model/token cost
    List {
        /// One JSON array for agent drivers (bd-cron-run-no-live-u9tg)
        #[arg(long)]
        json: bool,
    },
    /// Show one run's step trace / outputs
    Show {
        /// run id (the log entry id)
        id: String,
    },
    /// Runs parked on approval / spawn-await (pending)
    Pending,
    /// Digest: failures + parked + broken > N runs, since a cutoff
    Digest {
        /// only consider entries after this time (ISO or "N[smhd]" ago)
        #[arg(long)]
        since: Option<String>,
        /// a workflow/pipeline is "broken" after this many consecutive failures
        #[arg(long, default_value_t = 3)]
        broken_after: u32,
    },
}

//! `flywheel work` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum WorkCmd {
    /// bv --robot-triage: full graph triage with recommendations
    Triage,
    /// bv --robot-next: the single top pick
    Next,
    /// bv --robot-insights: graph metrics
    Insights,
    /// bv --robot-plan: parallel execution tracks
    Plan,
    /// br ready --json: ready beads
    Ready,
    /// bd-bead-create-contract-lint-4tdd: creation-time contract check —
    /// runs the EXACT loop parsers (verify fence + PRINCIPLES for P<=2)
    /// over the ready set and reports per-bead block reasons with the
    /// corrected snippet. Exit non-zero when any bead is not dispatchable.
    Check {
        /// project dir to run `br ready` in (default: current directory)
        project: Option<String>,
    },
}

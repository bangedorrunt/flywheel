//! `flywheel cron` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// `flywheel cron` subcommands (ADR-0012 Spec 1/4, P12.3): the standalone
/// cron surface over cron YAMLs. Crons are YAML files in three tiers —
/// per-project > global > built-in — with a shared CronSpec body on the
/// toron-workflow engine. `lint`/`run` validate + test a cron on demand;
/// `start` drives the durable loops; pause/resume/remove mutate the tier's
/// YAML (built-ins are read-only; shadow by a user YAML instead).
#[derive(Subcommand)]
pub(crate) enum CronCmd {
    /// List resolved crons (per-project > global > built-in) with source +
    /// scope
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one cron's resolved definition (spec + source + scope)
    Show {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Lint cron YAML(s): engine tier (schedule parse, policy sanity, step/
    /// graph checks) + flywheel tier (owner-profile resolution). Exit 0
    /// clean, 1 on any blocking issue, 2 on a parse failure.
    Lint {
        /// lint only this cron (name), else every resolved cron
        name: Option<String>,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Run a cron's body on demand (testing): lint gate first, then the spec
    /// steps through the shared engine with the spec's mode runner.
    Run {
        /// cron name (resolved tier)
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// REALLY dispatch the body through the spec's mode runner (the same
        /// headless/pane path the durable loop fires) instead of the
        /// structural dry-run (bd-cron-run-no-live-u9tg)
        #[arg(long)]
        live: bool,
    },
    /// Pause a cron (entry preserved; nothing fires) — persists `enabled:
    /// false` in the tier's YAML
    Pause {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Resume a paused cron (removes the `enabled: false` marker)
    Resume {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Status of every cron: paused | due | running | succeeded | failed
    Status {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Run history for a cron (state-log scan + failure beads)
    History {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Start (or resume) the durable cron loops over the resolved crons.
    /// Health-gate probes + `on_failure: bead` are live. Blocks; Ctrl-C to
    /// stop.
    Start {
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Remove a cron definition (user tiers only; built-ins are read-only).
    /// Removing a project-tier cron reveals the global/built-in definition
    /// it shadowed.
    Remove {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
}

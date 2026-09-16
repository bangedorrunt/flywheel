//! `flywheel stage` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// `flywheel stage` subcommands (ADR-0005 P3.3; ADR-0022 §3.8 W6): the
/// layered stage registry surface.
#[derive(Subcommand)]
pub(crate) enum StageCmd {
    /// List stages (seed + authored; `--all` kept as an alias)
    List {
        /// include seed (default: true; flag kept so old `--all` scripts still parse)
        #[arg(long)]
        all: bool,
        /// JSON rows (agent flag parity; list is JSON when this is set)
        #[arg(long)]
        json: bool,
    },
    /// Show one stage: prompt body + metadata + tier
    Show {
        /// stage name (semantic or p-code alias)
        name: String,
    },
    /// Scaffold a new stage into the project tier (.flywheel/stages.toml)
    New {
        /// stage name (must be a valid identifier)
        name: String,
    },
    /// Lint every tier: shape, size cap, role affinity, phase, principles
    Lint,
    /// Promote a stage up the §3.8 permanence ladder (project → user → repo)
    Promote {
        /// stage name (must resolve at project or user tier)
        name: String,
        /// target tier: user | repo
        #[arg(long, value_parser = ["user", "repo"])]
        to: String,
        /// repo tier only: >=2 successful run ids as evidence (distinct
        /// objectives, verified against the durable run log)
        #[arg(long, value_delimiter = ',')]
        evidence: Vec<String>,
        /// repo tier only: the review bead id witnessing the promotion
        #[arg(long)]
        review_bead: Option<String>,
        /// repo tier only: flywheel repo checkout holding src/repo-stages.toml
        /// (default: $FLYWHEEL_REPO_DIR)
        #[arg(long)]
        repo_dir: Option<String>,
    },
}

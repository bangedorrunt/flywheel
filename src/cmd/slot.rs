//! `flywheel slot` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Routing slot subcommands (ADR-0007 D1/D4). A slot is harness + model
/// (+ fallbacks) for a cost band: cheap | standard | premium.
#[derive(Subcommand)]
pub(crate) enum SlotCmd {
    /// List slots with source (compiled-in vs user overlay)
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Show one slot (harness, model, fallbacks, source)
    Show {
        /// slot name or alias (cheap/free/smol, standard/default, premium/slow/plan)
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Set slot fields in the USER models.toml (whole-Spec overlay; the
    /// compiled-in defaults are never baked into the file — ADR-0007 D1).
    Set {
        /// slot name: cheap | standard | premium (never "default" — that is
        /// the compiled-in squad)
        name: String,
        /// harness / program (opencode | pi | omp | grok | ...)
        #[arg(long)]
        harness: Option<String>,
        /// concrete model id (provider/model or bare; NOT @alias)
        #[arg(long)]
        model: Option<String>,
        /// fallback harness when the primary is unavailable
        #[arg(long)]
        fallback_harness: Option<String>,
        /// ordered model-id fallbacks (comma-separated)
        #[arg(long = "model-fallback", value_delimiter = ',')]
        model_fallback: Vec<String>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the USER overlay for a slot (restores the compiled-in spec)
    Remove {
        /// slot name
        name: String,
        /// skip the confirm
        #[arg(long)]
        force: bool,
    },
}

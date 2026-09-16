//! `flywheel squad` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Squad commands (ADR-0005 P6): a squad is a named group of profile counts —
/// the generalization of the legacy `Recipe`. `squad spawn <recipe>` resolves
/// each member profile via `Profile::resolve` and spawns it on the backend.
#[derive(Subcommand)]
pub(crate) enum SquadCmd {
    /// List squads (compiled-in + from recipes.toml as aliases)
    List,
    /// Show a squad's member profiles + counts
    Show {
        /// squad / recipe name
        name: String,
    },
    /// Spawn every profile in the squad (resolves via Profile::resolve)
    Spawn {
        /// squad / recipe name (a legacy recipe maps via recipe_to_squad)
        name: String,
        /// project scope for resolve (default: host scope / operator)
        #[arg(long)]
        project: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Remove a squad/recipe from ~/.config/flywheel/recipes.toml.
    Remove {
        /// squad / recipe name
        name: String,
    },
    /// Create/overwrite a squad in ~/.config/flywheel/recipes.toml
    /// (ADR-0007 D3: user file overrides compiled-in squads per name).
    Create {
        /// squad name
        name: String,
        /// member counts: --profile coder=3 (repeatable)
        #[arg(long = "profile", value_name = "PROFILE=COUNT", value_parser = crate::parse_kind_count)]
        members: Vec<(String, u32)>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
}

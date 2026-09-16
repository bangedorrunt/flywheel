//! `flywheel model` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// `flywheel model` subcommands (ADR-0010 Spec D, P10.2): the `models.toml`
/// registry. Profile.model ids resolve against it for per-harness injection;
/// unknown ids warn and pass through (never block a run).
#[derive(Subcommand)]
pub(crate) enum ModelCmd {
    /// List registered model ids
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Show one registry entry (provider, cost, caps, fallback)
    Show {
        /// model id (e.g. claude-sonnet-4)
        id: String,
    },
    /// Add or update a registry entry in ~/.config/flywheel/models.toml
    Add {
        /// model id
        id: String,
        /// provider prefix (`anthropic` → opencode -m anthropic/<id>)
        #[arg(long)]
        provider: String,
        /// input cost per million tokens (advisory)
        #[arg(long, default_value_t = 0.0)]
        cost_in_per_mtok: f64,
        /// output cost per million tokens (advisory)
        #[arg(long, default_value_t = 0.0)]
        cost_out_per_mtok: f64,
        /// harnesses this model supports (comma list; empty = all)
        #[arg(long)]
        caps: Option<String>,
        /// fallback id when a harness is outside caps — ALSO the retry-on-429
        /// fallback (bd-42fs)
        #[arg(long)]
        fallback: Option<String>,
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// cost tier (ADR-0007): cheap | standard | premium
        #[arg(long)]
        tier: Option<String>,
    },
    /// Set which slot a model serves as the DEFAULT for (ADR-0007 D1:
    /// "select default model from CLI"). Writes the whole SlotSpec to the
    /// user models.toml overlay — never bakes compiled-ins.
    Set {
        /// model id (provider/model or bare; bare resolves via the
        /// default provider; must be a registered id)
        id: String,
        /// target slot: cheap | standard | premium (TTY without --slot
        /// prompts; grok is closed-source — it has no slot)
        #[arg(long)]
        slot: Option<String>,
        /// never prompt (non-interactive)
        #[arg(long)]
        no_input: bool,
        /// show what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Set the operator-wide default model pair (bd-ll3k): every slot
    /// alias (@cheap/@standard/@premium) resolves through it while set.
    /// Writes `[defaults]` to the user models.toml overlay — explicit
    /// per-profile harness and concrete model ids still win (CLI >
    /// concrete id > defaults > slot).
    SetDefault {
        /// concrete model id (provider/model or bare; must be registered)
        #[arg(long)]
        model: Option<String>,
        /// harness (pi | omp | opencode | grok | ...)
        #[arg(long)]
        harness: Option<String>,
        /// never prompt (non-interactive; missing halves are an error)
        #[arg(long)]
        no_input: bool,
        /// show what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the effective default pair and which aliases it overrides.
    Defaults {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Clear the operator-wide default (back to per-slot behavior).
    ClearDefault {
        /// actually clear (skip the confirm)
        #[arg(long)]
        force: bool,
        /// never prompt (non-interactive)
        #[arg(long)]
        no_input: bool,
    },
    /// Remove registry entries. Bare ids resolve via the default provider.
    /// Compiled-in ids are refused unless --shadow (writes a user shadow).
    /// TTY with no ids → dialoguer MultiSelect. Requires --force to delete.
    Remove {
        /// model ids (bare or provider/model; repeatable)
        ids: Vec<String>,
        /// resolve bare ids against the default provider (default: on)
        #[arg(long, default_value_t = true)]
        resolve: bool,
        /// remove compiled-in ids by writing nothing — they cannot be
        /// deleted; this flag is accepted so scripts do not break
        #[arg(long)]
        shadow: bool,
        /// actually delete (skip the confirm)
        #[arg(long)]
        force: bool,
        /// never prompt (non-interactive)
        #[arg(long)]
        no_input: bool,
    },
}

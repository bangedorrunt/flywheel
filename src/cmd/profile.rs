//! `flywheel profile` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// `flywheel profile` subcommands (ADR-0005 Spec 1/1b) — the agent catalog.
/// `list`/`show` are read-only; `create`/`edit` write `profiles.toml`; `run`
/// resolves `@name` → `Backend::spawn` (host scope, operator identity) and
/// optionally invokes a workflow.
#[derive(Subcommand)]
pub(crate) enum ProfileCmd {
    /// List catalog profiles (compiled-in defaults + locals)
    List,
    /// Show a profile (full behavior preamble + skills)
    Show {
        /// profile name (e.g. housekeeper, or a local [profiles.foo])
        name: String,
        /// machine output with the ADR-0007 D1 resolved route
        /// (harness/model after slot-inherit + fallback ladder)
        #[arg(long)]
        json: bool,
    },
    /// Add or update a profile in ~/.config/flywheel/profiles.toml
    Create {
        /// profile name
        name: String,
        /// harness / program (pi | oc | grok | hermes | prime-agent | ...)
        #[arg(long)]
        harness: Option<String>,
        /// model id (e.g. claude-sonnet-4)
        #[arg(long)]
        model: Option<String>,
        /// role label (free-form prompt-shaping)
        #[arg(long)]
        role: Option<String>,
        /// behavior preamble (system-prompt discipline)
        #[arg(long)]
        behavior: Option<String>,
        /// short task hint
        #[arg(long)]
        task_hint: Option<String>,
        /// backend trait impl (default herdr)
        #[arg(long)]
        backend: Option<String>,
        /// default workflow name
        #[arg(long)]
        default_workflow: Option<String>,
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// omp role model for lightweight tasks (--smol=, bd-gsvn.2)
        #[arg(long)]
        smol: Option<String>,
        /// omp role model for slow/thorough reasoning (--slow=, bd-gsvn.2)
        #[arg(long)]
        slow: Option<String>,
        /// omp role model for architectural planning (--plan=, bd-gsvn.2)
        #[arg(long)]
        plan: Option<String>,
    },
    /// Alias for `create` (same fields)
    Edit {
        /// profile name
        name: String,
        /// harness / program
        #[arg(long)]
        harness: Option<String>,
        /// model id
        #[arg(long)]
        model: Option<String>,
        /// role label
        #[arg(long)]
        role: Option<String>,
        /// behavior preamble
        #[arg(long)]
        behavior: Option<String>,
        /// short task hint
        #[arg(long)]
        task_hint: Option<String>,
        /// backend trait impl
        #[arg(long)]
        backend: Option<String>,
        /// default workflow name
        #[arg(long)]
        default_workflow: Option<String>,
        /// thinking/effort level for omp (--thinking=): minimal|low|medium|
        /// high|xhigh|max (bd-42fs)
        #[arg(long)]
        effort: Option<String>,
        /// omp role model for lightweight tasks (--smol=, bd-gsvn.2)
        #[arg(long)]
        smol: Option<String>,
        /// omp role model for slow/thorough reasoning (--slow=, bd-gsvn.2)
        #[arg(long)]
        slow: Option<String>,
        /// omp role model for architectural planning (--plan=, bd-gsvn.2)
        #[arg(long)]
        plan: Option<String>,
    },
    /// Resolve @name (host scope) and spawn the agent via the configured
    /// backend. Optionally invoke a workflow after spawn.
    Run {
        /// profile name (e.g. housekeeper)
        name: String,
        /// workflow to invoke after spawn (resolves via the workflow catalog)
        #[arg(long)]
        workflow: Option<String>,
        /// JSON inputs for the workflow invoke
        #[arg(long)]
        inputs: Option<String>,
    },
    /// Remove a local profile from ~/.config/flywheel/profiles.toml (built-ins
    /// cannot be removed — override them via `create`/`edit` instead).
    Remove {
        /// profile name
        name: String,
    },
    /// Set routing fields on a profile (ADR-0007 D4 — the missing verb).
    /// Persists the merged profile to the user profiles.toml (bd-2dao rule:
    /// write the RAW user file merged over the built-in, never the catalog
    /// rebuild).
    Set {
        /// profile name (built-in or local)
        name: String,
        /// model id or slot alias (@cheap/@standard/@premium)
        #[arg(long)]
        model: Option<String>,
        /// provider prefix for a concrete model id
        #[arg(long)]
        provider: Option<String>,
        /// slot alias override for model (same as --model @slot)
        #[arg(long)]
        slot: Option<String>,
        /// ordered model-id fallbacks (comma-separated; NOT Profile.fallback —
        /// those are other profile names, ADR-0026)
        #[arg(long = "model-fallback", value_delimiter = ',')]
        model_fallback: Vec<String>,
        /// ordered profile-name fallbacks for billing/auth/provider reroute
        /// (ADR-0026 Spec 3). Distinct from --model-fallback.
        #[arg(long = "fallback", value_delimiter = ',')]
        fallback: Vec<String>,
        /// harness / program (empty on a slot-aliased profile inherits the
        /// slot's harness)
        #[arg(long)]
        harness: Option<String>,
        /// thinking/effort level: minimal|low|medium|high|xhigh|max
        #[arg(long)]
        effort: Option<String>,
        /// omp --smol= role model (id or @slot)
        #[arg(long)]
        smol: Option<String>,
        /// omp --slow= role model (id or @slot)
        #[arg(long)]
        slow: Option<String>,
        /// omp --plan= role model (id or @slot)
        #[arg(long)]
        plan: Option<String>,
        /// comma-separated skills list (catalog only)
        #[arg(long, value_delimiter = ',')]
        skills: Vec<String>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Export a profile as the versioned portable doc (ADR-0013 P2-11) to
    /// stdout, or a file with --out
    Export {
        /// profile name (built-in or local; exports the merged definition)
        name: String,
        /// write the doc to this path instead of stdout
        #[arg(long)]
        out: Option<String>,
    },
    /// Import a portable profile doc: upsert into
    /// ~/.config/flywheel/profiles.toml (an import of a built-in name lands
    /// as a deliberate local override)
    Import {
        /// path to the portable doc, or - for stdin
        path: String,
        /// import under this name instead of the doc's own
        #[arg(long)]
        name: Option<String>,
        /// print the resulting user-file entry without writing
        #[arg(long)]
        dry_run: bool,
    },
}

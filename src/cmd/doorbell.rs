//! `flywheel doorbell` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Subcommands of `flywheel doorbell` (bd-durable-doorbell-lngf): the
/// supervised pane-wake layer surface.
#[derive(Subcommand)]
pub(crate) enum DoorbellCmd {
    /// per-workspace liveness: dispatcher up/down, last wake, unanswered
    /// pins. Lazily heals dead dispatchers while reporting.
    Status { project: String },
    /// start/attach dispatchers for every registered workspace that ever
    /// ran one (the crash-recovery entry point).
    Start { project: String },
    /// stop dispatchers (per workspace; `all` for every registered one)
    Stop {
        project: String,
        /// workspace id, or `all`
        #[arg(default_value = "all")]
        workspace: String,
    },
    /// re-inject the peer-aware handler + identity line into EXISTING panes
    /// (covers herdr pane-split additions and post-crash respawns).
    ReInject { project: String },
    /// batch captain-relevant wakes since the last digest (ADR-0010 D5).
    /// Folds tick from Default over the doorbell journal (same trust
    /// surface as `why`); urgent wakes never batch. Wedge alarm when the
    /// queue cannot drain.
    Afk {
        project: String,
        /// newest entries to show (bd-z5ut: the folded queue is unbounded by
        /// construction — an older remainder is summarized, never silently
        /// dropped)
        #[arg(long, default_value = "100")]
        limit: usize,
    },
    /// generalized digest (bd-asr2.11, D8): per-identity wake digest over
    /// any window — earliest first, unanswered escalated on top. `afk`
    /// stays as the captain-default alias. Roster-free (no arrival triple).
    Digest {
        project: String,
        /// identity to digest for (pin, subscriber tag, or npub); empty = all
        #[arg(long)]
        identity: Option<String>,
        /// only rows at/after this RFC3339 timestamp
        #[arg(long)]
        since: Option<String>,
        /// output format: text (default) or json
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// why did (or didn't) a pin get rung? Replays the doorbell journal —
    /// GS-007 bd-yw4s: the trust surface, not log tailing. Folds tick from Default.
    Why {
        project: String,
        /// pin (adjective+noun) to explain (use with --corr/--seq for assignment replay)
        pin: String,
        /// max entries to show (newest last)
        #[arg(long, default_value = "20")]
        limit: usize,
        /// replay by corr id (assignment) — folds tick from Default, not grep
        #[arg(long)]
        corr: Option<String>,
        /// replay by seq — folds tick from Default up to this seq
        #[arg(long)]
        seq: Option<u64>,
        /// replay only rows tagged for this subscriber (bd-asr2.11);
        /// untagged legacy rows are excluded under a filter
        #[arg(long)]
        subscriber: Option<String>,
    },
    /// operator console inject (D8, bd-woha): journal a Fact row plus queue
    /// one pending wake — digest-visible with zero swarm. Same-second
    /// ratchet + persisted per-pin 5/30/3 budget. The console is the
    /// operator (local loopback invocation), so no separate token.
    Inject {
        project: String,
        /// pin (adjective+noun) to ring
        pin: String,
        /// wake line
        line: String,
        /// journal scope (default `manual` — console injections are their
        /// own scope, never mixed into a workspace journal)
        #[arg(long, default_value = "manual")]
        workspace: String,
    },
    /// subscribe an identity to wake delivery (D8, bd-woha): identity ×
    /// scope × sink × budget row in the subscription store. Cross-workspace
    /// scopes and non-herdr sinks require the operator (refused loudly).
    Subscribe {
        /// home workspace
        #[arg(long)]
        workspace: String,
        /// pin (adjective+noun) to subscribe
        #[arg(long)]
        pin: String,
        /// sink: herdr (default), webhook, dm (endpoint binds at the adapter)
        #[arg(long, default_value = "herdr")]
        sink: String,
        /// where this row's wake lands (bd-8ulu): the webhook endpoint URL,
        /// e.g. `http://127.0.0.1:18999/wake`. Per-row, so two subscribers
        /// can target two endpoints. The host must be on the daemon's
        /// `DOORBELL_WEBHOOK_ALLOWLIST` or the daemon never registers it
        /// (the wake then stays digest-visible).
        #[arg(long)]
        url: Option<String>,
        /// extra workspaces for an explicit cross-workspace scope
        /// (repeatable; operator-only)
        #[arg(long)]
        extra: Vec<String>,
        /// project context so the daemon can auto-host this scope
        /// (bd-2o1v); omitted = legacy row, never auto-hosted
        #[arg(long)]
        project: Option<String>,
    },
    /// list subscription store rows.
    Subscriptions {},
    /// drop one subscription row by id.
    Unsubscribe {
        /// subscription id (see `subscriptions`)
        id: String,
    },
}

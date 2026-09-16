//! `flywheel coalition` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// Coalition commands (ADR-0011 Spec 1, P11.3a): the steer surface + manual
/// collab. A coalition = NIP-29 channel + goal workflow run + manifest.
/// Manual collab reuses P11.2 (soft-conflict freeze + request-release) — no
/// auto-proposer here (P11.3b).
#[derive(Subcommand)]
pub(crate) enum CoalitionCmd {
    /// Register an agent identity in the coalition's toron project (members
    /// must be registered before `form`; profiles are not agents).
    Register {
        /// agent name (key name in the project's agents dir)
        name: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Form a captain-led coalition around a goal workflow: lint gate, start
    /// the goal run, create the NIP-29 channel, write the manifest.
    Form {
        /// goal workflow name (must pass the lint gate)
        goal: String,
        /// crew: comma-separated squads (recipes.toml) / profile names —
        /// one per project for cross-project coalitions (bd-hkw5.3)
        #[arg(long)]
        crews: String,
        /// act as this agent (the captain; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// cross-project scoping: <member>=<project> (repeatable; absent
        /// members default to --project) (bd-hkw5.3)
        #[arg(long = "member-project")]
        member_project: Vec<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// bead ids tracked for the beads-closed disband gate (comma list)
        #[arg(long)]
        beads: Option<String>,
        /// the repo whose .beads holds the tracked beads (br resolves its
        /// workspace from cwd; default: none — probe from the current dir)
        #[arg(long)]
        tracker: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Full coalition view: manifest + run + channel + intents + reservations
    /// + frozen paths + in-flight negotiations + bead gate.
    Status {
        /// coalition id (from form / list)
        id: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// compact one-line-per-member digest (bd-hkw5.1)
        #[arg(long)]
        digest: bool,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Declare status:intent into the coalition's intent board (P11.2).
    Intent {
        /// coalition id
        id: String,
        /// path globs the intent covers
        paths: Vec<String>,
        /// act as this agent (must be a member)
        #[arg(long = "as")]
        as_agent: String,
        #[arg(long)]
        reason: Option<String>,
        /// intent TTL in seconds (default 300)
        #[arg(long)]
        ttl: Option<u64>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Acquire leases on path globs (P11.2). A soft-conflict FREEZE against
    /// another member's intent lands in soft_conflicts — the manual-collab
    /// trigger.
    Reserve {
        /// coalition id
        id: String,
        /// path globs to reserve
        paths: Vec<String>,
        /// act as this agent (must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// lease TTL in seconds
        #[arg(long)]
        ttl: Option<u64>,
        /// exclusive lease (default: true)
        #[arg(long)]
        exclusive: bool,
        #[arg(long)]
        reason: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: ask the holder to release/defer (P11.2).
    NegotiateRequest {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the holder being asked (must be a member)
        #[arg(long)]
        target: String,
        /// reservation id (kind-30031 event id from reserve)
        #[arg(long)]
        reservation_id: String,
        /// ask: `release` | `defer:<secs>`
        #[arg(long)]
        ask: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: the holder answers (P11.2).
    NegotiateRespond {
        /// coalition id
        id: String,
        /// act as this agent (the holder; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        /// decision: `agreed` | `denied` | `deferred:<secs>`
        #[arg(long)]
        decision: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: negotiation resolution (pending / responded /
    /// escalation-due) (P11.2).
    NegotiateStatus {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        /// escalation tier window in seconds (default 900)
        #[arg(long)]
        window: Option<u64>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Request-release: escalate an unanswered request to the captain
    /// (kind-14 DM, P11.2).
    NegotiateEscalate {
        /// coalition id
        id: String,
        /// act as this agent (the requester; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// the request message id from negotiate-request
        #[arg(long)]
        request_message: String,
        #[arg(long)]
        note: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// ADR-0011 D5: join an active coalition (wires CoalitionStore::join).
    Join {
        id: String,
        #[arg(long = "as")]
        as_agent: String,
        #[arg(long)]
        member_project: Option<String>,
        #[arg(long, default_value = "toron")]
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Leave an active coalition.
    Leave {
        id: String,
        #[arg(long = "as")]
        as_agent: String,
        #[arg(long, default_value = "toron")]
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// List coalitions (id, goal, status).
    List {
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// P11.3b detector: scan the intent board for overlapping crews and
    /// propose tiered rule-splits (split-by-directory / sequence-by-
    /// dependency / merge) + flag auto-coalesce candidates (non-trivial
    /// source overlaps between unaffiliated crews).
    Detect {
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// P11.3b auto-coalesce: form an ad-hoc coalition around a goal workflow
    /// for every non-trivial overlap the detector flagged between
    /// unaffiliated crews.
    AutoCoalesce {
        /// goal workflow name (must pass the lint gate)
        goal: String,
        /// act as this agent (the captain; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Disband a coalition: beads-closed gate (when tracked), member
    /// reservations released, channel close post, manifest closed.
    Disband {
        /// coalition id
        id: String,
        /// require every tracked bead to be closed first (the primary
        /// disband path; manual disband is the auto-fallback)
        #[arg(long)]
        beads_closed: bool,
        #[arg(long)]
        reason: Option<String>,
        /// override the tracker repo recorded at form (br cwd for the
        /// beads-closed probe)
        #[arg(long)]
        tracker: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Dispatch a headless leaf worker (bd-hkw5.2): the captain pre-splits
    /// a non-overlapping scope; the leaf runs it one-shot (ADR-0010 Spec A
    /// subprocess-reuse), logs to runs/, and posts the result to the
    /// coalition channel. Leaf semantics — no intent-declare/negotiate.
    Leaf {
        /// coalition id
        id: String,
        /// the acting member (leaf worker; must be a member)
        #[arg(long = "as")]
        as_agent: String,
        /// profile ref the leaf runs under (@planner)
        #[arg(long)]
        profile: String,
        /// the captain's pre-split, non-overlapping scope
        #[arg(long)]
        scope: String,
        /// per-dispatch harness override (pi|omp|opencode|grok)
        #[arg(long)]
        harness: Option<String>,
        /// toron project slug (default: toron)
        #[arg(long, default_value = "toron")]
        project: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
}

//! `flywheel mail` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum MailCmd {
    /// fetch inbox (default: config identity; override with --as / FLYWHEEL_MAIL_AS / AGENT_NAME)
    Inbox {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        urgent: bool,
        #[arg(long)]
        unread: bool,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// only messages strictly newer than this RFC3339 timestamp, or the
        /// literal `last` (daemon-side crash-proof cursor, read-then-bump)
        #[arg(long)]
        since: Option<String>,
        /// keep only messages in this thread (thread id or message id)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// bd-mail-inbox-json-a7wc: emit a STABLE machine contract — one JSON
        /// array of rows, exactly these keys per row: id, thread_id, sender,
        /// subject, importance, ack_required, read_ts_us, created_ts_us,
        /// duplicate_of, body_md (nullable: thread_id/duplicate_of/
        /// read_ts_us). Without it the human/legacy dump is unchanged.
        #[arg(long)]
        json: bool,
    },
    /// reply to a message in its thread (thread = the message id or thread id)
    Reply {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// message id to reply to (toron hex event id / thread id)
        #[arg(long = "message-id")]
        message_id: String,
        #[arg(long)]
        body: String,
        /// act as this agent (required for crew peer mail; else CoralFinch/config)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// mark a message read (read receipt; the projection honors it once
    /// read-state lands toron-side — bd-5icg)
    Read {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// message id (toron hex event id)
        #[arg(long = "message-id")]
        message_id: String,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// acknowledge a message that had ack_required=true
    Ack {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// message id (toron hex event id)
        #[arg(long = "message-id")]
        message_id: String,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// FTS5 search over the project's indexed messages
    Search {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// FTS5 query (phrase/prefix/boolean; hyphenated ids are quoted)
        query: String,
        #[arg(long, default_value = "20")]
        limit: u32,
        /// filter by sender (adjective+noun or herdr name)
        #[arg(long)]
        sender: Option<String>,
        /// filter by importance: low|normal|high|urgent
        #[arg(long)]
        importance: Option<String>,
        /// filter to one thread (thread id or message id)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        /// newest first instead of relevance
        #[arg(long)]
        recency: bool,
        /// include message bodies in results
        #[arg(long)]
        bodies: bool,
    },
    /// summarize a thread (participants, key points, action items)
    Summarize {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// thread id or message id
        #[arg(long = "thread-id")]
        thread_id: String,
        /// include up to 3 sample messages
        #[arg(long)]
        examples: bool,
    },
    /// resolve an agent's profile (npub, program, model, task)
    Whois {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// agent (adjective+noun or herdr short name)
        agent: String,
    },
    /// register/refresh an agent identity (upsert). One operator is one
    /// identity (bd-register-portability-3c2b): a name is REQUIRED — bind an
    /// existing key with --from-project/--adopt-unnamed instead of minting.
    Register {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// agent name (adjective+noun pin; REQUIRED unless importing)
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "flywheel")]
        program: String,
        #[arg(long, default_value = "flywheel")]
        model: String,
        /// short task description shown in listings
        #[arg(long)]
        task: Option<String>,
        /// bind this operator's EXISTING key from another project's registry
        /// (`{data}/{slug}/agents/<name>/key.ncryptsec`) instead of minting
        /// a second npub
        #[arg(long)]
        from_project: Option<String>,
        /// adopt a prior unnamed mint (`agents/key.ncryptsec`) under this
        /// name — moves the key so register reuses its npub
        #[arg(long)]
        adopt_unnamed: Option<String>,
    },
    /// renew this identity's file reservations (extend the TTL)
    Renew {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// seconds to extend from the current expiry (default 3600)
        #[arg(long, default_value = "3600")]
        extend_seconds: u64,
        /// restrict to these path patterns (default: all of this identity's)
        #[arg(long)]
        paths: Vec<String>,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// operator force-release of a stale reservation (herdr-down / abandoned)
    ForceUnlock {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// path (or glob) of the stale reservation to revoke — the daemon
        /// resolves the active lease on this path itself (bd-cumj)
        #[arg(long = "paths", value_name = "PATH")]
        path: String,
        /// holder agent name to disambiguate (optional)
        #[arg(long)]
        holder: Option<String>,
        /// optional note for the released holder
        #[arg(long)]
        note: Option<String>,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// build slots: acquire/renew/release the advisory 'cargo' slot
    Slot {
        #[command(subcommand)]
        cmd: MailSlotCmd,
    },
    /// set this agent's inbound contact policy
    Policy {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// policy: open|auto|contacts_only|block_all
        #[arg(long)]
        policy: String,
        /// agent whose policy to set (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// per-agent mail digest: unread/urgent counts + last sender/subject
    /// (orchestrator "who has mail" view; default = own identity)
    Digest {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// digest every registered agent (table, --json = array)
        #[arg(long)]
        all: bool,
    },
    /// wait until a thread gains a new message (blocks; Ctrl-C to abort)
    Watch {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// thread id or message id to watch
        #[arg(long = "thread")]
        thread_id: String,
        /// act as this registered agent (default: config identity)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// seconds to wait before giving up (default 300; 0 = forever)
        #[arg(long, default_value = "300")]
        timeout_secs: u64,
        /// poll interval seconds (default 5)
        #[arg(long, default_value = "5")]
        interval_secs: u64,
    },
    /// send a message to registered agents
    Send {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// recipients: comma-separated pins/herdr shorts; REQUIRED unless
        /// --crew expands them for you (bd-mail-crew-fanout-xq6b)
        #[arg(long)]
        to: Option<String>,
        /// bd-mail-crew-fanout-xq6b: expand the project spawn registry into
        /// recipients — every live crew pin gets one addressed copy (sender
        /// excluded, deduped, --to entries unioned on top). The broadcast-
        /// free alternative toron's rejection error names.
        #[arg(long = "crew")]
        crew: bool,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long, default_value = "normal")]
        importance: String,
        #[arg(long)]
        ack: bool,
        /// act as this agent (required for crew peer mail; else CoralFinch/config)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// thread id (use bead id for collab threads)
        #[arg(long = "thread-id")]
        thread_id: Option<String>,
        /// GS-039 P0-1: wake tag on the gift-wrap (`standing`|`tasked`).
        /// Default `tasked` so mail deliveries are not wake:null theater.
        #[arg(long = "wake", default_value = "tasked")]
        wake: String,
        /// ADR-0010 D9 (bd-l0fz, bd-7qg6): stamp the structural done-hold
        /// (`bead=<id>` extra tag). The loop's dispatch guard matches the TAG,
        /// never the `[bd-###] done` subject — a done subject without this tag
        /// never fires the hold. In-process mailbox only (refuses loud remote).
        #[arg(long = "resolve-bead")]
        resolve_bead: Option<String>,
    },
    /// reserve / inspect file locks (advisory; subcommands: reserve, list, show)
    Locks {
        #[command(subcommand)]
        cmd: MailLockCmd,
    },
    /// release this identity's reservations
    Unlock {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// check reservation conflicts for paths
    Conflicts {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        paths: Vec<String>,
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// list registered agents in the project (active within 24h by default;
    /// --all shows the full history incl. stale registrations)
    Agents {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// include ALL registered identities (history), not just active ones
        #[arg(long)]
        all: bool,
    },
    /// cross-project contact (request_contact / respond / list)
    Contact {
        #[command(subcommand)]
        cmd: MailContactCmd,
    },
    /// ADR-0010 D8 (bd-d8e4): corr-preserving resend. Re-runs the send under
    /// the SAME corr — toron's idempotent claim dedupes within TTL and the
    /// receipt stays corr-keyed. Refuses resulted (resend would duplicate)
    /// and degraded (bookkeeping failed after enqueue — likely delivered).
    /// A plain re-run of `mail send` mints a NEW corr and duplicates.
    Resend {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// the corr stamped on the original send (from receipts / the
        /// transport-unknown print)
        #[arg(long)]
        corr: String,
        /// recipients: comma-separated pins (the original delivery set)
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        /// act as this agent (the original sender pin)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// product grouping for multi-project search/inbox
    Product {
        #[command(subcommand)]
        cmd: MailProductCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum MailLockCmd {
    /// reserve files (advisory locks)
    Reserve {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        paths: Vec<String>,
        #[arg(long, default_value = "7200")]
        ttl_seconds: u64,
        #[arg(long)]
        exclusive: bool,
        #[arg(long, default_value = "flywheel")]
        reason: String,
        /// hold locks as this agent (crew must pass --as $AGENT_NAME)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// dump the active lock board (path, holder, exclusive, reason, expires, id)
    List {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// filter to one holder's leases
        #[arg(long)]
        holder: Option<String>,
    },
    /// show the holders of one path/glob (glob-overlap match)
    Show {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        /// path or glob whose holders to show
        path: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum MailContactCmd {
    /// request contact with an agent (optionally in another project)
    Request {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        to: String,
        /// target project path for cross-project contact
        #[arg(long)]
        to_project: Option<String>,
        #[arg(long, default_value = "cross-project coordination")]
        reason: String,
    },
    /// accept a pending contact request
    Approve {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        from: String,
    },
    /// deny a pending contact request
    Deny {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        from: String,
    },
    /// list contact links for the flywheel identity
    List {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
    },
    /// set contact policy: open|auto|contacts_only|block_all
    Policy {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long)]
        policy: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum MailSlotCmd {
    /// acquire the advisory build slot (exclusive recommended)
    Acquire {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "cargo")]
        slot: String,
        #[arg(long)]
        exclusive: bool,
        #[arg(long, default_value = "1800")]
        ttl_seconds: u64,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// extend an active slot lease
    Renew {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "cargo")]
        slot: String,
        #[arg(long, default_value = "1800")]
        extend_seconds: u64,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// release an active slot lease (non-destructive)
    Release {
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
        #[arg(long, default_value = "cargo")]
        slot: String,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum MailProductCmd {
    /// ensure a product exists (e.g. trading-research, knowledge)
    Ensure {
        #[arg(long)]
        name: String,
    },
    /// link a project into a product
    Link {
        #[arg(long)]
        product: String,
        project: Option<String>,
        #[arg(long = "project", value_name = "PROJECT")]
        project_flag: Option<String>,
    },
    /// search messages across a product
    Search {
        #[arg(long)]
        product: String,
        query: String,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// product-scoped inbox for the flywheel identity
    Inbox {
        #[arg(long)]
        product: String,
        #[arg(long, default_value = "20")]
        limit: u32,
    },
}

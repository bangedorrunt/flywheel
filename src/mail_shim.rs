//! In-process agent-mail shim (ADR-0003 Decision 3A/3B, bd-o8j.2/.3).
//!
//! `src/mail.rs` (the HTTP JSON-RPC client to the agent-mail MCP server) was
//! deleted. This shim keeps the `Mail` surface call sites depend on (method
//! signatures + JSON shapes) and routes every operation through one of two
//! backends:
//!
//! - IN-PROCESS (default): the `toron_core::mailbox::Mailbox` (InProcessBus +
//!   SQLite index, an operator key attached so `ensure_project`/send/register
//!   work) — zero HTTP, one mailbox per process, built eagerly at the first
//!   `Mail` construction (`init_mailbox`, se2.1 Phase A).
//! - REMOTE-DAEMON (OPTIONAL, bd-o8j.3): a standalone `toron serve --mcp-http
//!   <addr>` over Streamable HTTP (MCP), driven by a minimal MCP client
//!   (`crate::mcp_http::McpClient`). Selected when `[agent_mail] url` is set.
//!
//! flywheel main.rs dispatch is sync, so each in-process op is `block_on`'d on
//! a shared tokio runtime (the remote path is ureq-sync, no runtime needed).
//! The pane-identity bridge helpers (`project_hash`,
//! `write_pane_identity`, …) are LOCAL filesystem ops regardless of backend —
//! they bridge flywheel's own herdr panes, which only the local process sees.
//!
//! Known limitations (reported, not papered over):
//! - toron message ids are hex event ids, not u64 sequence numbers;
//! - `ensure_product` needs an operator key; `demo_mailbox` has none.
//!
//! governed-by: ADR-0003

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::runtime::Runtime;

use toron_core::mailbox::{
    ContactPolicy, FetchInbox, ListAgentsOptions, RequestContact, RespondContact, SearchMessages,
    SendMessage, overlaps,
};
use toron_core::nostr::identity::AgentProfile;
use toron_mcp::ToJson;

use crate::config::MailConfig;
use crate::mcp_http::{McpClient, ServerKind};

/// Shared tokio runtime: ONE per process (c3b.1 / se2.1 Phase A), built by
/// `init_mailbox` — never a fresh `Runtime::new` per op. Construction failure
/// is captured in the `Result`, so it propagates as an error at
/// `Mail::from_config`, never a panic mid-op.
static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

/// The single shared in-process mailbox every `Mail` instance operates on
/// (ADR-0003 Decision 3A). Mirrors `toron_mcp::demo_mailbox` (InProcessBus +
/// SQLite index) but WITH an operator key, so write paths (`ensure_project`,
/// send, register) work — the demo mailbox is operator-less and would reject
/// every `ensure_project` on first use. Root dirs follow the durable D3
/// resolution (bd-t9ey): `$XDG_DATA_HOME/toron` / `~/.local/share/toron` by
/// default, `TORON_DATA_DIR` / `TORON_MCP_DATA_DIR` override.
static MAILBOX: OnceLock<Result<toron_core::mailbox::Mailbox, String>> = OnceLock::new();

fn runtime() -> Result<&'static Runtime> {
    RUNTIME
        .get_or_init(|| {
            Runtime::new().map_err(|e| format!("failed to initialize tokio runtime: {e}"))
        })
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

/// Build the shared runtime + mailbox eagerly, propagating failures as errors
/// (c3b.1: no panic on runtime/threadpool construction failure). Called from
/// `Mail::from_config`; every in-process op then reads the pre-built statics.
fn init_mailbox() -> Result<()> {
    let _rt = runtime()?;
    match MAILBOX.get_or_init(|| match runtime() {
        Ok(rt) => rt.block_on(build_mailbox()).map_err(|e| format!("{e:#}")),
        Err(e) => Err(e.to_string()),
    }) {
        Ok(_) => Ok(()),
        Err(e) => bail!("flywheel mail: in-process mailbox: {e}"),
    }
}

/// The shared in-process mailbox. Only reachable after a successful
/// `init_mailbox` (via `Mail::from_config`) — an unreachable panic here means
/// a call path skipped construction, which is a programming error.
fn mailbox() -> &'static toron_core::mailbox::Mailbox {
    match MAILBOX.get() {
        Some(Ok(mb)) => mb,
        Some(Err(e)) => {
            unreachable!("mailbox init failure must surface via Mail::from_config: {e}")
        }
        None => unreachable!("mailbox() before Mail::from_config"),
    }
}

/// Root dirs for the in-process mailbox (se2.1 Phase B, bd-t9ey): the
/// durable D3 root via `toron_core::config::resolve_data_root` (ADR-0007
/// D3) — `TORON_DATA_DIR` > `$XDG_DATA_HOME/toron` > `~/.local/share/toron`
/// > relative `toron-data`. NEVER a per-pid temp root: separate flywheel
/// > invocations share one durable mailbox. The `TORON_MCP_DATA_DIR` alias
/// > stays as the explicit demo/test isolation override. The SQLite index
/// > lives at the root. Returns (data_dir, sqlite db path).
pub(crate) fn mailbox_roots() -> (PathBuf, PathBuf) {
    // `TORON_DATA_DIR` wins (blank counts as unset — flywheel's old trim
    // semantics); `TORON_MCP_DATA_DIR` is the demo/test isolation alias;
    // otherwise the durable D3 default via resolve_data_root.
    let toron_data_dir = std::env::var("TORON_DATA_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    if toron_data_dir.is_none()
        && let Ok(dir) = std::env::var("TORON_MCP_DATA_DIR")
        && !dir.trim().is_empty()
    {
        let root = PathBuf::from(dir.trim());
        return (root.clone(), root.join("flywheel-mail.sqlite"));
    }
    let env = toron_core::config::DataRootEnv {
        toron_data_dir,
        xdg_data_home: env_non_empty_os("XDG_DATA_HOME"),
        home: env_non_empty_os("HOME"),
    };
    let root = toron_core::config::resolve_data_root(&env);
    (root.clone(), root.join("flywheel-mail.sqlite"))
}

/// Mirror of toron's `env_non_empty` for the ambient os-string vars: empty
/// counts as unset, so `resolve_data_root` never sees a blank root.
fn env_non_empty_os(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

async fn build_mailbox() -> anyhow::Result<toron_core::mailbox::Mailbox> {
    let (data_dir, db) = mailbox_roots();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create mail data dir {}", data_dir.display()))?;
    let db_path = db.to_string_lossy().into_owned();
    let pool = toron_core::index::schema::connect(&db_path).await?;
    toron_core::index::schema::migrate(&pool).await?;
    let bus = toron_core::bus::InProcessBus::open(
        pool.clone(),
        toron_core::bus::in_process::InProcessBusConfig::default(),
    )
    .await?;
    let operator = toron_core::keys::generate();
    Ok(
        toron_core::mailbox::Mailbox::new(std::sync::Arc::new(bus), data_dir, Some(operator))
            .with_index_pool(pool),
    )
}

/// Run one async mailbox operation on the shared tokio runtime (flywheel
/// main.rs dispatch is sync; each op is a short-lived, sequential block_on).
/// The runtime is pre-built by `Mail::from_config` — the expect below is
/// unreachable post-init (c3b.1: no `Runtime::new` per op).
pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
    runtime()
        .expect("runtime initialized by Mail::from_config before any op")
        .block_on(f)
}

/// Full inbox filter bag (bd-mail-ack-sla-tracking). Callers construct this
/// instead of passing 5 booleans positionally. Constructed by the toron
/// backend (`mail-toron` feature); suppressed as dead in the default build.
#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct MailInboxFilter {
    /// Keep only messages with importance ∈ {high, urgent} (c3b.3 — applied
    /// by the toron projection; rows carry `importance`).
    pub urgent_only: bool,
    pub unread_only: bool,
    /// Not implementable on the toron projection (rows carry no ack_ts):
    /// requesting it fails with an honest error, never a silent no-op (c3b.3).
    pub ack_overdue_only: bool,
    pub include_bodies: bool,
    pub limit: u32,
    /// Keep only messages with `created_ts_us` strictly newer than this
    /// RFC3339 timestamp (c3b.3 — applied by the toron projection).
    pub since_ts: Option<String>,
}

/// c3b.3: parse an RFC3339 `since_ts` filter into epoch micros (the unit
/// `InboxMessage.created_ts_us` uses). A bad timestamp is an error — never a
/// silent "filter ignored".
fn parse_since_ts_us(ts: &str) -> Result<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_micros())
        .map_err(|e| anyhow!("inbox since_ts {ts:?} is not RFC3339: {e}"))
}

/// c3b.3: `urgent_only` membership — importance ∈ {high, urgent}.
fn is_urgent(importance: &str) -> bool {
    matches!(importance, "high" | "urgent")
}

/// Filter bag for `Mail::search` (review-fix F8: folds the 10 positional args
/// into a struct — the trait's `SearchFilter` is the backend-agnostic twin).
/// Constructed by the toron backend (`mail-toron` feature); suppressed as dead
/// in the default build.
#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct MailSearchFilter {
    pub sender: Option<String>,
    pub importance: Option<String>,
    pub thread_id: Option<String>,
    /// Not yet applied by the toron projection (parity surface kept).
    #[allow(dead_code)]
    pub date_start: Option<String>,
    /// Not yet applied by the toron projection (parity surface kept).
    #[allow(dead_code)]
    pub date_end: Option<String>,
    pub ranking: Option<String>,
    pub include_body_md: bool,
}

/// Which backend a `Mail` routes through.
#[derive(Debug, Clone)]
enum BackendKind {
    /// Integrated toron mailbox in this process (the default, bd-o8j.2).
    InProcess,
    /// Standalone `toron serve --mcp-http` daemon over Streamable HTTP.
    Remote(McpClient),
}

/// Agent-mail facade. Every public method maps 1:1 onto either a
/// `toron_core::mailbox::Mailbox` call (in-process) or a toron MCP `tools/call`
/// against a remote daemon (see the deleted `src/mail.rs` for the surface this
/// replaces). JSON return shapes are identical across backends.
///
/// This is the TORON facade (ADR-0003 V2). Its full method surface is consumed
/// by the feature-gated toron backend (`mail-toron`) plus the CLI mail
/// commands; in the default build (toron backend off) the ops the CLI doesn't
/// call are dead — suppressed so the default build stays warning-free.
#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct Mail {
    /// The agent identity this facade sends AS / reads the inbox for.
    pub name: String,
    /// Active backend (in-process default; remote when `url` is set).
    backend: BackendKind,
}

#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
impl Mail {
    /// Rebind the facade's sender identity. Construction-time only: consumes
    /// self so `Mail` stays `&self`-callable afterwards (c3b.2 — no interior
    /// mutability needed; `name` is never written through `&self`).
    pub fn with_name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    /// Construct the mail facade from `[agent_mail]` config. Picks the
    /// backend: `cfg.url` Some → remote-daemon (bd-o8j.3); None/unset →
    /// integrated in-process mailbox (default). The config section stays
    /// OPTIONAL — no config means the integrated mailbox with identity
    /// `flywheel`.
    pub fn from_config(cfg: Option<&MailConfig>) -> Result<Option<Self>> {
        match cfg.and_then(|c| c.url.as_deref()) {
            Some(url) if !url.is_empty() => Self::remote(cfg),
            _ => {
                // Eager shared-runtime + mailbox build (se2.1 Phase A / c3b.1):
                // failures propagate as errors here — no panic on runtime or
                // mailbox construction failure, and every op reuses the one
                // pre-built runtime.
                init_mailbox()?;
                Ok(Some(Mail {
                    name: cfg
                        .and_then(|c| c.name.clone())
                        .unwrap_or_else(|| "flywheel".to_string()),
                    backend: BackendKind::InProcess,
                }))
            }
        }
    }

    /// Construct a REMOTE-daemon-backed facade: `cfg.url` must be Some
    /// (`http://host:port` of a `toron serve --mcp-http` daemon). `Ok(None)`
    /// when no url is set — callers treat that as mail-not-configured.
    pub fn remote(cfg: Option<&MailConfig>) -> Result<Option<Self>> {
        let name = cfg
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| "flywheel".to_string());
        let Some(url) = cfg.and_then(|c| c.url.clone()) else {
            return Ok(None);
        };
        let client = McpClient::new(
            &url,
            cfg.and_then(|c| c.token.clone()),
            ServerKind::Toron, // this facade IS the toron transport (ADR-0003 V1)
        )
        .map_err(|e| anyhow::anyhow!("agent-mail remote daemon {url}: {e:#}"))?;
        Ok(Some(Mail {
            name,
            backend: BackendKind::Remote(client),
        }))
    }

    /// Route a tool call through the remote daemon (in-process backend errors).
    fn call_remote(&self, tool: &str, args: Value) -> Result<Value> {
        match &self.backend {
            BackendKind::Remote(client) => {
                // McpError (Transport|Tool) -> anyhow via the blanket From.
                client.call_tool(tool, args).map_err(anyhow::Error::from)
            }
            BackendKind::InProcess => bail!("{tool} requires the remote-daemon backend"),
        }
    }

    /// `health_check` (in-process) / `health_check` (remote).
    pub fn health(&self) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let report = block_on(mailbox().health_check());
                Ok(report.to_json())
            }
            BackendKind::Remote(_) => {
                // `project_key` is unused by the toron tool; empty string.
                let mut v =
                    self.call_remote("health_check", crate::backend::wire::health_check_args())?;
                v["transport"] = json!("http"); // distinguishing marker: remote mode
                Ok(v)
            }
        }
    }

    /// `ensure_project`.
    pub fn ensure_project(&self, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let state = block_on(mailbox().ensure_project(project_key))?;
                Ok(state.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "ensure_project",
                crate::backend::wire::ensure_project_args(project_key),
            ),
        }
    }

    /// `list_agents` (project's registered identities). `active_within_days`
    /// filters to agents active within that window (surface parity — the
    /// daemon supports it on the wire; in-process passes it to the mailbox).
    pub fn list_agents(&self, project_key: &str, active_within_days: Option<u64>) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let agents = block_on(
                    mailbox().list_agents(project_key, ListAgentsOptions { active_within_days }),
                )?;
                Ok(Value::Array(agents.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "list_agents",
                crate::backend::wire::list_agents_args(project_key, active_within_days),
            ),
        }
    }

    /// `whois` (project + agent name → profile). Surface parity — the mailbox
    /// owns name resolution (G2); the daemon tool of the same name.
    pub fn whois(&self, project_key: &str, agent_name: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let who = block_on(mailbox().whois(project_key, agent_name))?;
                Ok(who.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "whois",
                crate::backend::wire::whois_args(project_key, agent_name),
            ),
        }
    }

    /// `register_agent` (upsert on the agent's key).
    pub fn register_agent(
        &self,
        project_key: &str,
        name: &str,
        program: &str,
        model: &str,
        task_description: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let mut profile = AgentProfile::new(name);
                profile.program = Some(program.to_string());
                profile.model = Some(model.to_string());
                profile.task_description = Some(task_description.to_string());
                let agent = block_on(mailbox().register_agent(project_key, &profile))?;
                Ok(agent.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "register_agent",
                crate::backend::wire::register_agent_args(
                    project_key,
                    name,
                    program,
                    model,
                    task_description,
                ),
            ),
        }
    }

    /// `fetch_inbox` (simple filter set).
    pub fn inbox(
        &self,
        project_key: &str,
        agent_name: &str,
        _urgent_only: bool,
        limit: u32,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: false,
                    limit: Some(limit as usize),
                    include_bodies: true,
                    urgent_only: false,
                    since_ts: None,
                }))?;
                Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "fetch_inbox",
                crate::backend::wire::inbox_args(
                    project_key,
                    agent_name,
                    false,
                    false,
                    false,
                    true,
                    limit,
                    None,
                ),
            ),
        }
    }

    /// `fetch_inbox` with the full filter bag (c3b.3). Applies every filter
    /// the toron row model supports: `urgent_only` (rows carry `importance`)
    /// and `since_ts` (rows carry `created_ts_us`), on top of the already-
    /// supported `unread_only` / `include_bodies` / `limit`. `ack_overdue_only`
    /// has NO toron projection (rows carry no ack_ts) — it fails with an
    /// honest `Unsupported` instead of silently returning an unfiltered
    /// inbox; the ack-SLA escalation uses the dedicated `ack_overdue()`.
    pub fn inbox_filtered(
        &self,
        project_key: &str,
        agent_name: &str,
        filter: &MailInboxFilter,
    ) -> Result<Value> {
        if filter.ack_overdue_only {
            return Err(anyhow!(
                "toron inbox_filtered: ack_overdue_only is not supported by the toron projection \
                 (inbox rows carry no ack_ts) — use the dedicated ack_overdue() surface"
            ));
        }
        let since_us = filter
            .since_ts
            .as_deref()
            .map(parse_since_ts_us)
            .transpose()?;
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: filter.unread_only,
                    limit: Some(filter.limit as usize),
                    include_bodies: filter.include_bodies,
                    urgent_only: filter.urgent_only,
                    since_ts: filter.since_ts.clone(),
                }))?;
                let rows = msgs
                    .iter()
                    .filter(|m| !filter.urgent_only || is_urgent(&m.importance))
                    .filter(|m| match since_us {
                        Some(s) => m.created_ts_us > s,
                        None => true,
                    })
                    .map(ToJson::to_json)
                    .collect();
                Ok(Value::Array(rows))
            }
            BackendKind::Remote(_) => {
                let v = self.call_remote(
                    "fetch_inbox",
                    crate::backend::wire::inbox_args(
                        project_key,
                        agent_name,
                        filter.urgent_only,
                        filter.unread_only,
                        filter.ack_overdue_only,
                        filter.include_bodies,
                        filter.limit,
                        filter.since_ts.as_deref(),
                    ),
                )?;
                // Same policy on the wire rows (InboxMessageOut carries
                // `importance` + `created_ts_us`): filter client-side.
                let rows = v
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|row| {
                        !filter.urgent_only
                            || row
                                .get("importance")
                                .and_then(Value::as_str)
                                .map(is_urgent)
                                .unwrap_or(false)
                    })
                    .filter(|row| match since_us {
                        Some(s) => row
                            .get("created_ts_us")
                            .and_then(Value::as_i64)
                            .map(|ts| ts > s)
                            .unwrap_or(true),
                        None => true,
                    })
                    .collect();
                Ok(Value::Array(rows))
            }
        }
    }

    /// `mark_message_read`.
    pub fn mark_read(
        &self,
        project_key: &str,
        agent_name: &str,
        message_id: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                // toron ids are hex event ids — pass through verbatim.
                let r = block_on(mailbox().mark_message_read(project_key, agent_name, message_id))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => {
                // The remote daemon is toron (`[mail.toron] url`): its
                // message ids are hex event ids and its `mark_message_read`
                // tool takes a String — pass the id through verbatim, never
                // coerce it to a number (se2.2; the AM backend in
                // agentmail.rs owns the decimal-id edge).
                self.call_remote(
                    "mark_message_read",
                    crate::backend::wire::mark_read_args(
                        project_key,
                        agent_name,
                        json!(message_id),
                    ),
                )
            }
        }
    }

    /// `acknowledge_message`.
    pub fn ack(&self, project_key: &str, agent_name: &str, message_id: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                // toron ids are hex event ids — pass through verbatim.
                let r =
                    block_on(mailbox().acknowledge_message(project_key, agent_name, message_id))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => {
                // Same as mark_message_read above: the toron daemon's
                // `acknowledge_message` takes the hex event id as a String —
                // pass it through verbatim (se2.2).
                self.call_remote(
                    "acknowledge_message",
                    crate::backend::wire::ack_args(project_key, agent_name, json!(message_id)),
                )
            }
        }
    }

    /// Messages past the ack SLA. toron has no ack-overdue projection yet —
    /// returns the inbox (empty after all are acked), so the escalation
    /// protocol's "nothing overdue" branch behaves as before.
    #[allow(dead_code)] // consumed by the Phase-4 escalation protocol + live test
    pub fn ack_overdue(&self, project_key: &str, agent_name: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: false,
                    limit: None,
                    include_bodies: false,
                    urgent_only: false,
                    since_ts: None,
                }))?;
                Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "fetch_inbox",
                crate::backend::wire::inbox_args(
                    project_key,
                    agent_name,
                    false,
                    false,
                    false,
                    false,
                    0,
                    None,
                ),
            ),
        }
    }

    /// `send_message`.
    #[allow(clippy::too_many_arguments)] // mirrors the MailBackend::send trait surface
    pub fn send(
        &self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack: bool,
        thread_id: Option<&str>,
    ) -> Result<Value> {
        // bd-pncn: `all` (the broadcast token) is expanded to the project's
        // registered roster BEFORE the backend call — the toron surfaces
        // (in-process mailbox + daemon tool) reject a literal "all"
        // recipient. Zero registered agents is a clear error.
        let targets = self.expand_broadcast(project_key, to)?;
        match &self.backend {
            BackendKind::InProcess => {
                let result = block_on(mailbox().send_message(&SendMessage {
                    project_slug: project_key.to_string(),
                    sender_name: self.name.clone(),
                    to: targets,
                    subject: subject.to_string(),
                    body_md: body.to_string(),
                    importance: Some(importance.to_string()),
                    ack_required: ack,
                    thread_id: thread_id.map(String::from),
                    ..Default::default()
                }))?;
                Ok(result.to_json())
            }
            BackendKind::Remote(_) => {
                let args = crate::backend::wire::send_args(
                    project_key,
                    &self.name,
                    &targets,
                    subject,
                    body,
                    importance,
                    ack,
                    thread_id,
                );
                self.call_remote("send_message", args)
            }
        }
    }

    /// bd-pncn: expand the `all` broadcast token into the project's
    /// registered agent roster (backend-agnostic — `list_agents` serves both
    /// the in-process mailbox and the remote daemon). Explicit recipients
    /// pass through untouched; an explicit name also present in the roster
    /// is deduped (one gift-wrap per agent). Errors when the roster is empty.
    fn expand_broadcast(&self, project_key: &str, to: &[String]) -> Result<Vec<String>> {
        let has_broadcast = to.iter().any(|t| t.trim() == "all");
        if !has_broadcast {
            return Ok(to.to_vec());
        }
        let roster = self.list_agents(project_key, None)?;
        let mut out: Vec<String> = roster
            .as_array()
            .ok_or_else(|| anyhow!("list_agents returned a non-array: {roster}"))?
            .iter()
            .filter_map(|a| a.get("name").and_then(Value::as_str).map(String::from))
            .collect();
        if out.is_empty() {
            bail!(
                "cannot broadcast to 'all': project {project_key} has zero registered agents (register agents first, or name recipients explicitly)"
            );
        }
        for t in to {
            let t = t.trim();
            if t != "all" && !out.iter().any(|n| n == t) {
                out.push(t.to_string());
            }
        }
        Ok(out)
    }

    /// `reply_message` (threaded).
    pub fn reply(
        &self,
        project_key: &str,
        message_id: &str,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
        // toron's reply_message REQUIRES explicit recipients (no AM-style
        // "default to original sender") — resolve the parent's sender from
        // the inbox when the caller did not pass `to` (the common case:
        // `flywheel mail reply --message-id X --body Y`).
        let to: Option<Vec<String>> = match to {
            Some(t) if !t.is_empty() => Some(t.to_vec()),
            _ => self.parent_sender(project_key, message_id)?,
        };
        match &self.backend {
            BackendKind::InProcess => {
                // toron: the id IS the thread anchor (hex event id or the
                // free-form thread id — both flow through the `thread` tag).
                let result = block_on(mailbox().reply_message(&SendMessage {
                    project_slug: project_key.to_string(),
                    sender_name: self.name.clone(),
                    to: to.clone().unwrap_or_default(),
                    subject: String::new(),
                    body_md: body.to_string(),
                    thread_id: Some(message_id.to_string()),
                    ..Default::default()
                }))?;
                Ok(result.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "reply_message",
                crate::backend::wire::reply_args(
                    project_key,
                    &self.name,
                    &to.unwrap_or_default(),
                    // c3b.5: reply subject populated from the parent (`Re: `);
                    // was hardcoded `""` (unverified wire contract).
                    &self.parent_reply_subject(project_key, message_id)?,
                    body,
                    "thread_id",
                    json!(message_id),
                ),
            ),
        }
    }

    /// Resolve the sender of the message being replied to (for toron, which
    /// requires explicit reply recipients). Looks the parent up in the
    /// project inbox; returns `None` when the parent cannot be found (the
    /// caller then fails with a clear message instead of a silent drop).
    fn parent_sender(&self, project_key: &str, message_id: &str) -> Result<Option<Vec<String>>> {
        let v = self.inbox_filtered(
            project_key,
            &self.name,
            &MailInboxFilter {
                unread_only: false,
                include_bodies: false,
                limit: 100,
                ..Default::default()
            },
        )?;
        let arr = v.as_array().cloned().unwrap_or_default();
        for m in &arr {
            let id = m
                .get("id")
                .or_else(|| m.get("message_id"))
                .and_then(|i| i.as_str());
            let thread = m.get("thread_id").and_then(|t| t.as_str());
            if id == Some(message_id) || thread == Some(message_id) {
                let sender = m
                    .get("sender")
                    .or_else(|| m.get("from"))
                    .and_then(|s| s.as_str())
                    .map(String::from);
                if sender.is_some() {
                    return Ok(sender.map(|s| vec![s]));
                }
            }
        }
        Ok(None)
    }

    /// c3b.5: resolve the `Re: <parent subject>` subject for a reply. Looks
    /// the parent up in the project inbox (the message being replied to must
    /// be visible to this identity); falls back to `""` when the parent
    /// cannot be found — the wire builder still carries the subject field, so
    /// the hardcoded-empty contract is gone. Works for BOTH backends (the
    /// inbox fetch is the common projection).
    fn parent_reply_subject(&self, project_key: &str, message_id: &str) -> Result<String> {
        let v = self.inbox_filtered(
            project_key,
            &self.name,
            &MailInboxFilter {
                unread_only: false,
                include_bodies: false,
                limit: 500,
                ..Default::default()
            },
        )?;
        let parent = v.as_array().and_then(|rs| {
            rs.iter().find(|r| {
                r["id"].as_str() == Some(message_id) || r["thread_id"].as_str() == Some(message_id)
            })
        });
        Ok(match parent.and_then(|r| r["subject"].as_str()) {
            Some(subj) if !subj.is_empty() => format!("Re: {subj}"),
            _ => String::new(),
        })
    }

    /// `summarize_thread`.
    pub fn summarize_thread(
        &self,
        project_key: &str,
        thread_id: &str,
        include_examples: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let s = block_on(mailbox().summarize_thread(
                    project_key,
                    thread_id,
                    include_examples,
                    false,
                    None,
                ))?;
                Ok(s.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "summarize_thread",
                crate::backend::wire::summarize_thread_args(
                    project_key,
                    thread_id,
                    include_examples,
                ),
            ),
        }
    }

    /// `summarize_thread_product` — summarize a thread across a product's
    /// member projects (ADR-0003 V6 surface restore).
    pub fn summarize_thread_product(
        &self,
        product_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let s =
                    block_on(mailbox().summarize_thread_product(product_key, thread_id, llm_mode))?;
                Ok(s.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "summarize_thread_product",
                crate::backend::wire::summarize_thread_product_args(
                    product_key,
                    thread_id,
                    llm_mode,
                ),
            ),
        }
    }

    /// `search_messages`. The JSON envelope is `{"result": [...]}` so
    /// `harvest_latest` (debate / plan-review) reads `result[0].body_md`.
    pub fn search(
        &self,
        project_key: &str,
        query: &str,
        limit: u32,
        filter: &MailSearchFilter,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().search_messages(&SearchMessages {
                    project_key: project_key.to_string(),
                    query: query.to_string(),
                    limit: Some(limit as usize),
                    ranking: filter.ranking.clone(),
                    sender_name: filter.sender.clone(),
                    importance: filter.importance.clone(),
                    thread_id: filter.thread_id.clone(),
                    include_body_md: filter.include_body_md,
                    ..Default::default()
                }))?;
                Ok(json!({ "result": msgs.iter().map(ToJson::to_json).collect::<Vec<Value>>() }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "search_messages",
                    crate::backend::wire::search_args(
                        project_key,
                        query,
                        limit,
                        filter.include_body_md,
                        filter.sender.as_deref(),
                        filter.importance.as_deref(),
                        filter.thread_id.as_deref(),
                        filter.date_start.as_deref(),
                        filter.date_end.as_deref(),
                        filter.ranking.as_deref(),
                    ),
                )?;
                // The toron daemon already returns the envelope
                // (`{"result": [...], "next_cursor": ...}`); legacy
                // agent-mail returned a bare array. Pass the envelope
                // through when present, else wrap.
                Ok(match &res {
                    Value::Object(m) if m.contains_key("result") => res,
                    _ => json!({ "result": res }),
                })
            }
        }
    }

    /// Build the `search_messages` args JSON (pure parity surface; kept for
    /// callers that built args directly).
    #[allow(dead_code)] // parity surface (the old HTTP client exposed it)
    pub fn search_args(
        project_key: &str,
        query: &str,
        limit: u32,
        filter: &MailSearchFilter,
    ) -> Value {
        crate::backend::wire::search_args(
            project_key,
            query,
            limit,
            filter.include_body_md,
            filter.sender.as_deref(),
            filter.importance.as_deref(),
            filter.thread_id.as_deref(),
            filter.date_start.as_deref(),
            filter.date_end.as_deref(),
            filter.ranking.as_deref(),
        )
    }

    /// `file_reservation_paths`.
    pub fn reserve(
        &self,
        project_key: &str,
        agent_name: &str,
        paths: &[String],
        ttl_seconds: u64,
        exclusive: bool,
        reason: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().file_reservation_paths(
                    project_key,
                    agent_name,
                    paths,
                    Some(ttl_seconds),
                    exclusive,
                    Some(reason),
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "file_reservation_paths",
                crate::backend::wire::reserve_args(
                    project_key,
                    agent_name,
                    paths,
                    ttl_seconds,
                    exclusive,
                    reason,
                ),
            ),
        }
    }

    /// `release_file_reservations` (agent-scoped; `paths` empty = all own).
    pub fn release_paths(
        &self,
        project_key: &str,
        agent_name: &str,
        paths: &[String],
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r =
                    block_on(mailbox().release_file_reservations(project_key, agent_name, paths))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "release_file_reservations",
                crate::backend::wire::release_args(project_key, agent_name),
            ),
        }
    }

    /// `release_by_reason` (bd-h3oj): operator-signed revocation of every
    /// live lease whose `reason` tag is the bead id — the loop-side
    /// after-close hook. `released == 0` is a clean no-op.
    pub fn release_by_reason(
        &self,
        project_key: &str,
        requestor_name: &str,
        bead_id: &str,
        notify_previous: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().release_by_reason(
                    project_key,
                    requestor_name,
                    bead_id,
                    notify_previous,
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "release_file_reservations_by_reason",
                crate::backend::wire::release_by_reason_args(
                    project_key,
                    requestor_name,
                    bead_id,
                    notify_previous,
                ),
            ),
        }
    }

    /// `check_file_reservation_conflicts` (agent-scoped).
    pub fn conflicts_for(
        &self,
        project_key: &str,
        agent_name: &str,
        paths: &[String],
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().check_file_reservation_conflicts(
                    project_key,
                    agent_name,
                    paths,
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "check_file_reservation_conflicts",
                crate::backend::wire::conflicts_args(project_key, agent_name, paths),
            ),
        }
    }

    /// `active_reservations` (bd-dsu1 operator lock board): every active
    /// held lease in the project as JSON rows — path, holder, exclusive,
    /// reason, expiry, reservation id (the kind-30031 event id needed for
    /// diagnostics). Optional filters: `holder` (exact agent name) and
    /// `path` (glob overlap — shows the holders of one path). Read-only:
    /// never registers or publishes.
    pub fn list_active(
        &self,
        project_key: &str,
        holder: Option<&str>,
        path: Option<&str>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let rows = block_on(mailbox().active_reservations(project_key))?;
                let reservations: Vec<Value> = rows
                    .into_iter()
                    .filter(|r| holder.is_none_or(|h| r.agent_name == h))
                    .filter(|r| path.is_none_or(|p| overlaps(&r.path_pattern, p)))
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "path": r.path_pattern,
                            "holder": r.agent_name,
                            "exclusive": r.exclusive,
                            "reason": r.reason,
                            "expires_ts_us": r.expires_ts_us,
                        })
                    })
                    .collect();
                Ok(json!({
                    "project": project_key,
                    "count": reservations.len(),
                    "reservations": reservations,
                }))
            }
            BackendKind::Remote(_) => Err(anyhow::anyhow!(
                "locks list requires the in-process toron mailbox (the remote-daemon MCP surface has no active_reservations tool)"
            )),
        }
    }

    /// `renew_file_reservations`. The response keeps the `file_reservations`
    /// envelope `list_own_reservations` parses (ids are toron hex event ids).
    pub fn renew(
        &self,
        project_key: &str,
        agent_name: &str,
        extend_seconds: u64,
        paths: Option<&[String]>,
        _ids: Option<&[u64]>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().renew_file_reservations(
                    project_key,
                    agent_name,
                    paths.unwrap_or(&[]),
                    extend_seconds,
                ))?;
                Ok(json!({
                    "renewed": r.renewed,
                    "file_reservations": r.reservations.iter().map(|res| json!({
                        "id": res.id,
                        "path_pattern": res.path_pattern,
                        // bd-evqs: the reason (owning bead id) rides through
                        // the renew envelope so the loop can release
                        // reservations whose bead is CLOSED — the renewal
                        // storm fix needs the holder to know WHAT each lease
                        // is for.
                        "reason": res.reason,
                        "old_expires_ts": "",
                        "new_expires_ts": res.expires_ts_us.map(|u| u.to_string()).unwrap_or_default(),
                    })).collect::<Vec<Value>>(),
                }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "renew_file_reservations",
                    crate::backend::wire::renew_args(
                        project_key,
                        agent_name,
                        extend_seconds,
                        paths,
                    ),
                )?;
                // Remap the tool's `reservations` array onto the in-process
                // `file_reservations` envelope `list_own_reservations` reads.
                let mut out = json!({ "renewed": res.get("renewed").cloned().unwrap_or(json!(0)) });
                let fr = res
                    .get("reservations")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .map(|r| {
                                json!({
                                    "id": r.get("id").cloned().unwrap_or(json!(0)),
                                    "path_pattern": r.get("path_pattern").cloned().unwrap_or(json!("")),
                                    // bd-evqs: keep the reason through the
                                    // remap (the loop's closed-bead sweep
                                    // reads it to release stale leases).
                                    "reason": r.get("reason").cloned().unwrap_or(json!("")),
                                    "old_expires_ts": "",
                                    "new_expires_ts": r.get("expires_ts_us")
                                        .and_then(Value::as_u64)
                                        .map(|u| u.to_string())
                                        .unwrap_or_default(),
                                })
                            })
                            .collect::<Vec<Value>>()
                    })
                    .unwrap_or_default();
                out["file_reservations"] = json!(fr);
                Ok(out)
            }
        }
    }

    /// `force_release_file_reservation` — operator force-release of a stale
    /// reservation (ADR-0003 V6 surface restore). bd-cumj: resolves the lease
    /// by `path` (+ optional `holder`) — no caller-supplied event id. In-process
    /// requires the operator key (attached by `build_mailbox`); remote delegates
    /// to the daemon's tool.
    pub fn force_release_file_reservation(
        &self,
        project_key: &str,
        agent_name: &str,
        path: &str,
        holder: Option<&str>,
        note: Option<&str>,
        notify_previous: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                block_on(mailbox().force_release_file_reservation(
                    project_key,
                    agent_name,
                    path,
                    holder,
                    note,
                    notify_previous,
                ))?;
                Ok(json!({ "ok": true }))
            }
            BackendKind::Remote(_) => self.call_remote(
                "force_release_file_reservation",
                crate::backend::wire::force_release_args(
                    project_key,
                    agent_name,
                    path,
                    holder,
                    note,
                    notify_previous,
                ),
            ),
        }
    }

    /// `cleanup_pane_identities` — prune identities whose panes are absent
    /// from herdr's live pane list (ADR-0003 V6 surface restore). toron holds
    /// no per-pane identity files (live herdr mapping), so in-process "prune"
    /// reports the stale names.
    ///
    /// Honest errors (c3b.7): a herdr CONNECT failure when herdr IS the
    /// active multiplexer is a real outage — the caller must see Err, not an
    /// Ok note that reads like success. The Ok-note path fires ONLY for the
    /// genuinely-not-applicable configuration (active multiplexer is not
    /// herdr — the cleanup is herdr-specific; that is a config fact, not a
    /// failure).
    pub fn cleanup_pane_identities(&self, _project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                if !multiplexer_is_herdr() {
                    return Ok(json!({
                        "note": "active multiplexer is not herdr; pane identity cleanup not applicable",
                    }));
                }
                match toron_core::backend::herdr::HerdrBackend::connect(None) {
                    Ok(backend) => {
                        let pruned = mailbox().cleanup_pane_identities(&backend)?;
                        Ok(json!({ "pruned": pruned }))
                    }
                    Err(e) => Err(anyhow!(
                        "herdr unreachable; pane identity cleanup skipped: {e}"
                    )),
                }
            }
            BackendKind::Remote(_) => self.call_remote(
                "cleanup_pane_identities",
                crate::backend::wire::cleanup_pane_identities_args(_project_key),
            ),
        }
    }

    // ---- cross-project contact + products (bd-mail-cross-project-contact) ----

    /// `request_contact`.
    pub fn request_contact(
        &self,
        project_key: &str,
        from_agent: &str,
        to_agent: &str,
        to_project: Option<&str>,
        reason: &str,
        _ttl_seconds: Option<u64>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let link = block_on(mailbox().request_contact(&RequestContact {
                    project_slug: project_key.to_string(),
                    from_agent: from_agent.to_string(),
                    to_agent: to_agent.to_string(),
                    to_project: to_project.map(String::from),
                    reason: Some(reason.to_string()),
                }))?;
                Ok(link.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "request_contact",
                crate::backend::wire::request_contact_args(
                    project_key,
                    from_agent,
                    to_agent,
                    to_project,
                    reason,
                ),
            ),
        }
    }

    /// `respond_contact`.
    pub fn respond_contact(
        &self,
        project_key: &str,
        to_agent: &str,
        from_agent: &str,
        accept: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let link = block_on(mailbox().respond_contact(&RespondContact {
                    project_slug: project_key.to_string(),
                    to_agent: to_agent.to_string(),
                    from_agent: from_agent.to_string(),
                    accept,
                    from_project: None,
                }))?;
                Ok(link.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "respond_contact",
                crate::backend::wire::respond_contact_args(
                    project_key,
                    to_agent,
                    from_agent,
                    accept,
                ),
            ),
        }
    }

    /// `list_contacts`.
    pub fn list_contacts(&self, project_key: &str, agent_name: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let links = block_on(mailbox().list_contacts(project_key, agent_name))?;
                Ok(Value::Array(links.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "list_contacts",
                crate::backend::wire::list_contacts_args(project_key, agent_name),
            ),
        }
    }

    /// `set_contact_policy`.
    pub fn set_contact_policy(
        &self,
        project_key: &str,
        agent_name: &str,
        policy: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let id = block_on(mailbox().set_contact_policy(
                    project_key,
                    agent_name,
                    ContactPolicy::parse(policy),
                ))?;
                Ok(json!({ "policy": policy, "event_id": id }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "set_contact_policy",
                    crate::backend::wire::set_contact_policy_args(project_key, agent_name, policy),
                )?;
                Ok(json!({ "policy": policy, "event_id": res }))
            }
        }
    }

    /// `ensure_product` (idempotent product manifest; `name` defaults to the
    /// key when the caller has no separate display name).
    pub fn ensure_product(&self, product_key: &str, name: Option<&str>) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let info =
                    block_on(mailbox().ensure_product(product_key, name.unwrap_or(product_key)))?;
                Ok(info.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "ensure_product",
                crate::backend::wire::ensure_product_args(product_key),
            ),
        }
    }

    /// `products_link`.
    pub fn products_link(&self, product_key: &str, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let info = block_on(mailbox().products_link(product_key, project_key))?;
                Ok(info.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "products_link",
                crate::backend::wire::products_link_args(product_key, project_key),
            ),
        }
    }

    /// `search_messages_product`.
    pub fn search_product(&self, product_key: &str, query: &str, limit: u32) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let res = block_on(mailbox().search_messages_product(
                    product_key,
                    query,
                    Some(limit as usize),
                    true,
                ))?;
                Ok(json!({ "result": res.iter().map(|(p, m)| {
                    let mut v = m.to_json();
                    v["project"] = json!(p);
                    v
                }).collect::<Vec<Value>>() }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "search_messages_product",
                    crate::backend::wire::search_product_args(product_key, query, limit),
                )?;
                Ok(json!({ "result": res }))
            }
        }
    }

    /// `fetch_inbox_product`.
    pub fn inbox_product(&self, product_key: &str, agent_name: &str, limit: u32) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let res = block_on(mailbox().fetch_inbox_product(product_key, agent_name))?;
                Ok(
                    json!({ "messages": res.iter().take(limit as usize).map(|(p, m)| {
                    let mut v = m.to_json();
                    v["project"] = json!(p);
                    v
                }).collect::<Vec<Value>>() }),
                )
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "fetch_inbox_product",
                    crate::backend::wire::inbox_product_args(product_key, agent_name, limit),
                )?;
                let msgs = res
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .take(limit as usize)
                            .cloned()
                            .collect::<Vec<Value>>()
                    })
                    .unwrap_or_default();
                Ok(json!({ "messages": msgs }))
            }
        }
    }

    // ---- build slots (bd-mail-build-slot-gating) ----

    /// `acquire_build_slot`.
    pub fn acquire_slot(
        &self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
        exclusive: bool,
        ttl_seconds: u64,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().acquire_build_slot(
                    project_key,
                    agent_name,
                    slot,
                    Some(ttl_seconds),
                    exclusive,
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "acquire_build_slot",
                crate::backend::wire::slot_acquire_args(
                    project_key,
                    agent_name,
                    slot,
                    exclusive,
                    ttl_seconds,
                ),
            ),
        }
    }

    /// `renew_build_slot` (ADR-0003 V6: the `MailBackend::slot_renew` surface
    /// consumes this; no longer dead code).
    pub fn renew_slot(
        &self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
        extend_seconds: u64,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().renew_build_slot(
                    project_key,
                    agent_name,
                    slot,
                    extend_seconds,
                ))?;
                Ok(match r {
                    Some(b) => b.to_json(),
                    None => json!({ "slot": slot, "renewed": false }),
                })
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "renew_build_slot",
                    crate::backend::wire::slot_renew_args(
                        project_key,
                        agent_name,
                        slot,
                        extend_seconds,
                    ),
                )?;
                if res.is_null() {
                    Ok(json!({ "slot": slot, "renewed": false }))
                } else {
                    Ok(res)
                }
            }
        }
    }

    /// `release_build_slot`.
    pub fn release_slot(&self, project_key: &str, agent_name: &str, slot: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                block_on(mailbox().release_build_slot(project_key, agent_name, slot))?;
                Ok(json!({ "slot": slot, "released": true }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "release_build_slot",
                    crate::backend::wire::slot_release_args(project_key, agent_name, slot),
                )?;
                Ok(json!({
                    "slot": slot,
                    "released": res.get("released").cloned().unwrap_or(json!(true)),
                }))
            }
        }
    }

    // ---- pane identity bridge (bd-mail-pane-identity-bridge, ADR-0001 A4) ----

    /// Write the per-pane identity file the agent-mail server reads for
    /// `resolve_pane_identity`: `<config>/agent-mail/identity/<project_hash>/
    /// <pane_id>` containing `<name>\n`. A filesystem op, not an HTTP call —
    /// byte-for-byte what agent-mail core writes (verified against the live
    /// server 2026-08-05). Returns the path written.
    pub fn write_pane_identity(
        &self,
        project_key: &str,
        pane_id: &str,
        agent_name: &str,
    ) -> Result<PathBuf> {
        write_identity_for(project_key, pane_id, agent_name)
    }

    /// Remove the per-pane identity file (pane exited / respawned). No-op if
    /// absent — duplicate exit events must not fail. Returns the path.
    pub fn clear_pane_identity(&self, project_key: &str, pane_id: &str) -> Result<PathBuf> {
        clear_identity_for(project_key, pane_id)
    }

    /// Resolve the agent name for a pane from the canonical identity file
    /// (in-process read — the deleted HTTP client delegated to the agent-mail
    /// MCP tool, which read the same file). No file → a JSON without
    /// `agent_name` (callers treat that as not-found).
    pub fn resolve_pane_identity(&self, project_key: &str, pane_id: &str) -> Result<Value> {
        let path = canonical_identity_path(project_key, pane_id);
        match std::fs::read_to_string(&path) {
            Ok(name) if !name.trim().is_empty() => Ok(json!({
                "agent_name": name.trim(),
                "pane_id": pane_id,
                "identity_path": path.to_string_lossy(),
            })),
            _ => Ok(json!({
                "error": "no identity file",
                "pane_id": pane_id,
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// Pane identity files (bd-mail-pane-identity-bridge, ADR-0001 A4)
// ---------------------------------------------------------------------------

/// c3b.7: is herdr the ACTIVE multiplexer? Distinguishes "herdr is the
/// configured mux but the daemon is down" (a REAL failure the caller must
/// see) from "the mux is not herdr" (cleanup genuinely not applicable).
/// Mirrors factory precedence: `FLYWHEEL_BACKEND` env > `[backend]
/// multiplexer` config > default (`herdr`). Config load is best-effort —
/// no config file means the default applies.
fn multiplexer_is_herdr() -> bool {
    crate::config::Config::load()
        .map(|cfg| crate::backend::factory::resolve_backend_name(&cfg))
        .unwrap_or_else(|_| {
            // Config unreadable (e.g. tests without a config file): env or
            // the documented default.
            std::env::var("FLYWHEEL_BACKEND")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::backend::factory::DEFAULT_BACKEND.to_string())
        })
        == "herdr"
}

//
// flywheel's bridge for herdr panes: agent-mail writes per-pane identity
// files only for tmux panes; herdr panes (wT:p1) never get one, so
// `resolve_pane_identity` can't discover agents spawned by flywheel. The
// bridge writes the same file agent-mail core would have: 12-hex-char SHA-1
// of the canonicalized absolute project path as the directory name, the
// sanitized pane id as the filename, the agent name as a single trimmed line.
// All derivation + format details below were verified byte-for-byte against
// the live agent-mail server on 2026-08-05 (probe file resolved with the
// exact `identity_path` returned).

/// Config base for identity files: `$XDG_CONFIG_HOME` when set, else
/// `~/.config` — identical to agent-mail core's `config_base_dir()`, so the
/// bridge writes exactly where the server reads.
///
/// `#[cfg(test)]`: a process-global override pins the base to a temp dir so
/// tests never touch the real `~/.config` AND stay immune to parallel tests
/// that mutate `XDG_CONFIG_HOME`/`HOME` (the write+clear pair must resolve the
/// same path regardless of env races).
#[cfg(test)]
static TEST_IDENTITY_BASE: std::sync::LazyLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

fn identity_base() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = TEST_IDENTITY_BASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return path;
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg);
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from("/tmp/.config")
}

/// Truncated SHA-1 (lower hex, 12 chars) of the canonicalized absolute
/// project path — the exact derivation agent-mail core uses for
/// `identity/<project_hash>/`. Existing paths are canonicalized before
/// hashing (symlinked checkouts converge); a missing path is hashed as-is.
pub fn project_hash(project_key: &str) -> String {
    let key = PathBuf::from(project_key);
    let normalized = if key.is_absolute() {
        std::fs::canonicalize(&key).unwrap_or(key)
    } else {
        key
    };
    let mut hasher = Sha1::new();
    hasher.update(normalized.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize()).chars().take(12).collect()
}

/// Sanitize a pane id for use as a filename: strip a tmux `%` prefix, map
/// `:` to `-` and other filesystem-unsafe chars to `_` (agent-mail core
/// parity — herdr `w1:p1` → `w1-p1`). Empty → `unknown`.
fn sanitize_pane_id(pane_id: &str) -> String {
    let stripped = pane_id.strip_prefix('%').unwrap_or(pane_id);
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == ':' {
            out.push('-');
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// Canonical identity file path for a project + pane:
/// `<config>/agent-mail/identity/<project_hash>/<sanitized_pane_id>`.
pub fn canonical_identity_path(project_key: &str, pane_id: &str) -> PathBuf {
    identity_base()
        .join("agent-mail")
        .join("identity")
        .join(project_hash(project_key))
        .join(sanitize_pane_id(pane_id))
}

/// Write the identity file atomically (temp + rename), mirroring agent-mail
/// core's `write_identity_file` (0600, overwrite-only, no symlink).
pub(crate) fn write_identity_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename onto {}", path.display()))?;
    Ok(())
}

/// Write a per-pane identity file with the agent-mail-core guardrails (empty-
/// name refusal, symlink refusal). Shared by the toron `Mail` facade and the
/// real agent-mail backend (ADR-0003 V3) — both write the SAME file the
/// agent-mail server reads.
pub(crate) fn write_identity_for(
    project_key: &str,
    pane_id: &str,
    agent_name: &str,
) -> Result<PathBuf> {
    let name = agent_name.trim();
    if name.is_empty() {
        bail!("refusing to write empty pane identity for {pane_id}");
    }
    let path = canonical_identity_path(project_key, pane_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!(
            "refusing to overwrite symlinked pane identity {}",
            path.display()
        );
    }
    write_identity_file(&path, format!("{name}\n").as_bytes())?;
    Ok(path)
}

/// Remove a per-pane identity file (no-op when absent). Shared by the toron
/// `Mail` facade and the real agent-mail backend.
pub(crate) fn clear_identity_for(project_key: &str, pane_id: &str) -> Result<PathBuf> {
    let path = canonical_identity_path(project_key, pane_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(e) => Err(e).context("remove pane identity"),
    }
}

/// One renewed reservation, as reported by `renew_file_reservations`.
/// The id is a String: toron reservation ids are hex event ids, agent-mail's
/// are decimal — both pass through without numeric coercion (se2.2).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RenewedReservation {
    pub id: String,
    pub path_pattern: String,
    pub old_expires_ts: String,
    pub new_expires_ts: String,
}

/// Extract the renewed reservations from a `renew_file_reservations`
/// response. Agent-mail exposes no list-reservations MCP tool (verified
/// against the full tools/list surface), so the renew response is the
/// authoritative "what did I renew" source — the durable heartbeat logs this
/// list each tick (bd-mail-lease-renewal-durable).
pub fn list_own_reservations(resp: &Value) -> Vec<RenewedReservation> {
    let mut out = Vec::new();
    let Some(list) = resp.get("file_reservations").and_then(Value::as_array) else {
        return out;
    };
    for r in list {
        out.push(RenewedReservation {
            id: match r.get("id") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            },
            path_pattern: r
                .get("path_pattern")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            old_expires_ts: r
                .get("old_expires_ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            new_expires_ts: r
                .get("new_expires_ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_none_defaults_to_in_process_flywheel() {
        let mail = Mail::from_config(None)
            .expect("from_config ok")
            .expect("Some");
        assert_eq!(mail.name, "flywheel");
        assert!(matches!(&mail.backend, BackendKind::InProcess));
    }

    #[test]
    fn from_config_without_url_is_in_process() {
        let cfg = MailConfig {
            url: None,
            token: None,
            name: Some("orchestrator".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        assert_eq!(mail.name, "orchestrator");
        assert!(matches!(&mail.backend, BackendKind::InProcess));
    }

    #[test]
    fn from_config_with_empty_url_is_in_process() {
        let cfg = MailConfig {
            url: Some(String::new()),
            token: None,
            name: None,
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        assert!(matches!(&mail.backend, BackendKind::InProcess));
    }

    #[test]
    fn from_config_with_url_picks_remote() {
        let cfg = MailConfig {
            url: Some("http://127.0.0.1:19001".into()),
            token: Some("secret".into()),
            name: Some("orchestrator".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        assert_eq!(mail.name, "orchestrator");
        assert!(matches!(&mail.backend, BackendKind::Remote(_)));
    }

    #[test]
    fn remote_without_url_returns_none() {
        assert!(Mail::remote(None).expect("ok").is_none());
        let cfg = MailConfig {
            url: None,
            token: None,
            name: None,
        };
        assert!(Mail::remote(Some(&cfg)).expect("ok").is_none());
    }

    #[test]
    fn remote_with_url_constructs_facade() {
        let cfg = MailConfig {
            url: Some("http://127.0.0.1:19001".into()),
            token: None,
            name: Some("smoke".into()),
        };
        let mail = Mail::remote(Some(&cfg)).expect("ok").expect("Some");
        assert_eq!(mail.name, "smoke");
        assert!(matches!(&mail.backend, BackendKind::Remote(_)));
    }

    #[test]
    fn call_remote_on_in_process_backend_errors() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        assert!(mail.call_remote("health_check", json!({})).is_err());
    }

    /// se2.1 Phase A (c3b.1): all ops share ONE tokio runtime per process —
    /// `from_config` builds it eagerly, and ops run on it (no `Runtime::new`
    /// per op).
    #[test]
    fn block_on_uses_shared_runtime() {
        let _mail = Mail::from_config(None).expect("ok").expect("Some");
        assert!(
            RUNTIME.get().is_some(),
            "from_config initialized the runtime"
        );
        assert!(block_on(mailbox().ensure_project("se2.1-runtime-test")).is_ok());
        assert!(
            block_on(mailbox().ensure_project("se2.1-runtime-test-2")).is_ok(),
            "second op reuses the same runtime"
        );
    }

    /// se2.1 Phase B (bd-t9ey): the default root is DURABLE and XDG-based —
    /// `$XDG_DATA_HOME/toron` / `~/.local/share/toron` via
    /// `toron_core::config::resolve_data_root` (ADR-0007 D3) — NEVER a
    /// per-pid temp `flywheel-mail-{pid}.sqlite`. `TORON_DATA_DIR` still
    /// wins; `TORON_MCP_DATA_DIR` stays the demo/test isolation alias.
    #[test]
    fn mailbox_roots_env_and_default() {
        // Env mutation is process-global — serialize under the crate-wide
        // env lock (193.2); siblings (factory FLYWHEEL_MAIL, config XDG)
        // share the same lock.
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("TORON_DATA_DIR", "/tmp/flywheel-se2.1-durable") };
        unsafe { std::env::set_var("TORON_MCP_DATA_DIR", "/tmp/flywheel-se2.1-mcp") };
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/flywheel-se2.1-xdg") };
        let (data_dir, db) = mailbox_roots();
        assert_eq!(data_dir, PathBuf::from("/tmp/flywheel-se2.1-durable"));
        assert_eq!(
            db,
            PathBuf::from("/tmp/flywheel-se2.1-durable/flywheel-mail.sqlite")
        );
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/flywheel-se2.1-mcp"),
            "alias fallback"
        );
        unsafe { std::env::set_var("TORON_DATA_DIR", "   ") };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/flywheel-se2.1-mcp"),
            "blank counts as unset"
        );
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
        unsafe { std::env::remove_var("TORON_MCP_DATA_DIR") };
        let (data_dir, db) = mailbox_roots();
        assert_eq!(data_dir, PathBuf::from("/tmp/flywheel-se2.1-xdg/toron"));
        assert_eq!(
            db,
            PathBuf::from("/tmp/flywheel-se2.1-xdg/toron/flywheel-mail.sqlite")
        );
        assert!(
            !db.to_string_lossy()
                .contains(&format!("flywheel-mail-{}", std::process::id())),
            "no pid-scoped sqlite"
        );
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        unsafe { std::env::set_var("HOME", "/tmp/flywheel-se2.1-home") };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("/tmp/flywheel-se2.1-home/.local/share/toron")
        );
        unsafe { std::env::remove_var("HOME") };
        let (data_dir, db) = mailbox_roots();
        assert_eq!(
            data_dir,
            PathBuf::from("toron-data"),
            "no env at all → D3 relative fallback, never temp"
        );
        assert_eq!(db, PathBuf::from("toron-data/flywheel-mail.sqlite"));
    }

    /// se2.2: toron reservation ids are hex event ids — `list_own_reservations`
    /// must keep them as strings, never coerce a hex id to 0.
    #[test]
    fn list_own_reservations_keeps_hex_id() {
        let resp = json!({
            "renewed": 1,
            "file_reservations": [{
                "id": "a1b2c3d4e5f60718293a4b5c6d7e8f90",
                "path_pattern": "src/*.rs",
                "old_expires_ts": "",
                "new_expires_ts": "1750000000000000",
            }]
        });
        let out = list_own_reservations(&resp);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].id, "a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "hex id passes through untouched"
        );
        assert_ne!(out[0].id, "0", "hex id never becomes 0");
    }

    /// se2.2: agent-mail's numeric reservation ids surface as their decimal
    /// string — never "".
    #[test]
    fn list_own_reservations_keeps_numeric_id() {
        let resp = json!({
            "renewed": 1,
            "file_reservations": [{
                "id": 42,
                "path_pattern": "src/*.rs",
                "old_expires_ts": "",
                "new_expires_ts": "",
            }]
        });
        let out = list_own_reservations(&resp);
        assert_eq!(out[0].id, "42");
    }

    /// bd-dsu1: the operator lock board dumps every active lease (id +
    /// holder) and filters by holder / path-glob overlap. Read-only — no
    /// project creation, no identity registration.
    #[test]
    fn list_active_boards_and_filters() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "locks-list-board";
        mail.ensure_project(project).expect("ensure");
        for name in ["BlueLake", "RoseDune"] {
            mail.register_agent(project, name, "test", "test", "bd-dsu1 test")
                .expect("register");
        }
        mail.reserve(
            project,
            "BlueLake",
            &["src/**".into()],
            3600,
            true,
            "bd-dsu1",
        )
        .expect("BlueLake hold");
        mail.reserve(
            project,
            "RoseDune",
            &["docs/**".into()],
            3600,
            true,
            "bd-dsu1",
        )
        .expect("RoseDune hold");

        let board = mail.list_active(project, None, None).expect("board");
        assert_eq!(
            board["count"].as_u64().unwrap_or(0),
            2,
            "both leases on the board: {board}"
        );
        for row in board["reservations"].as_array().expect("rows") {
            assert!(
                !row["id"].as_str().unwrap_or("").is_empty(),
                "row carries the reservation id: {row}"
            );
            assert!(
                !row["holder"].as_str().unwrap_or("").is_empty(),
                "row names its holder: {row}"
            );
            assert!(
                row["exclusive"].as_bool().unwrap_or(false),
                "exclusive flag present: {row}"
            );
            assert_eq!(row["reason"].as_str().unwrap_or(""), "bd-dsu1");
        }

        let blue = mail
            .list_active(project, Some("BlueLake"), None)
            .expect("holder filter");
        assert_eq!(
            blue["count"].as_u64().unwrap_or(0),
            1,
            "holder filter narrows: {blue}"
        );
        assert_eq!(
            blue["reservations"][0]["path"].as_str().unwrap_or(""),
            "src/**"
        );

        let src = mail
            .list_active(project, None, Some("src/lib.rs"))
            .expect("path filter");
        assert_eq!(
            src["count"].as_u64().unwrap_or(0),
            1,
            "glob overlap narrows: {src}"
        );
        assert_eq!(
            src["reservations"][0]["holder"].as_str().unwrap_or(""),
            "BlueLake"
        );

        let none = mail
            .list_active(project, Some("Nobody"), None)
            .expect("no match");
        assert_eq!(
            none["count"].as_u64().unwrap_or(0),
            0,
            "unknown holder → empty board"
        );
    }

    /// S8.4: the swarm-start register_agent upsert clears stale profile task
    /// fields from a prior session. A 2026-08-14 identity still advertised
    /// `bd-qk9q`; re-registering the canonical name with the spawn profile
    /// must overwrite the task — never preserve the stale bead id.
    #[test]
    fn register_upsert_clears_stale_task_on_reuse() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "upsert-stale-task";
        mail.ensure_project(project).expect("ensure");
        // Prior session: the agent self-registered with a stale bead task.
        mail.register_agent(
            project,
            "flywheel-proj-oc",
            "opencode",
            "deepseek-v4-flash",
            "bd-qk9q implementer: stranded-bead reclaim fix",
        )
        .expect("prior session registration");
        // Swarm start: the spawn profile upserts the canonical name with a
        // fresh task stamp (S8.3/S8.4 — the same call register_spawned_agent
        // makes).
        let profile = crate::spawn_agent_profile("flywheel-proj-oc", "opencode", project);
        block_on(mailbox().register_agent(project, &profile)).expect("upsert");
        let who = mail.whois(project, "flywheel-proj-oc").expect("whois");
        let task = who["task_description"].as_str().unwrap_or("");
        assert!(
            task.contains("flywheel agent for upsert-stale-task"),
            "fresh task stamp wins: {task}"
        );
        assert!(!task.contains("bd-qk9q"), "stale bead task cleared: {task}");
    }

    /// bd-pncn: `--to all` broadcasts to every registered agent's inbox
    /// (roster expansion), not a literal "all" recipient.
    #[test]
    fn send_to_all_broadcasts_to_every_registered_agent() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "broadcast-all";
        mail.ensure_project(project).expect("ensure");
        for name in ["flywheel", "BlueLake", "RoseDune"] {
            mail.register_agent(project, name, "test", "test", "bd-pncn test")
                .expect("register");
        }
        let v = mail
            .send(
                project,
                &["all".into()],
                "br",
                "broadcast body",
                "normal",
                false,
                Some("bd-pncn"),
            )
            .expect("broadcast");
        assert_eq!(
            v["count"].as_u64().unwrap_or(0),
            3,
            "one delivery per registered agent: {v}"
        );
        for name in ["BlueLake", "RoseDune"] {
            let inbox = mail.inbox(project, name, false, 10).expect("inbox");
            assert!(
                inbox
                    .as_array()
                    .expect("rows")
                    .iter()
                    .any(|m| m["subject"] == "br"),
                "{name} inbox sees the broadcast: {inbox}"
            );
        }
    }

    /// bd-pncn: zero registered agents → clear error, never a silent no-op.
    #[test]
    fn send_to_all_with_zero_agents_errors() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "broadcast-empty";
        mail.ensure_project(project).expect("ensure");
        let err = mail
            .send(project, &["all".into()], "br", "x", "normal", false, None)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zero registered agents"),
            "clear error names the problem: {msg}"
        );
    }

    /// bd-pncn: `all` + explicit recipients → each agent delivered exactly
    /// once (dedupe).
    #[test]
    fn send_to_all_plus_explicit_dedupes() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = "broadcast-dedupe";
        mail.ensure_project(project).expect("ensure");
        for name in ["flywheel", "BlueLake", "RoseDune"] {
            mail.register_agent(project, name, "test", "test", "bd-pncn test")
                .expect("register");
        }
        let v = mail
            .send(
                project,
                &["all".into(), "BlueLake".into()],
                "br",
                "x",
                "normal",
                false,
                None,
            )
            .expect("send");
        assert_eq!(v["count"].as_u64().unwrap_or(0), 3, "deduped: {v}");
        let recipients: Vec<&str> = v["deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["payload"]["recipient"].as_str())
            .collect();
        assert_eq!(recipients.len(), 3);
        assert_eq!(
            recipients.iter().filter(|r| **r == "BlueLake").count(),
            1,
            "no double-delivery: {recipients:?}"
        );
    }

    /// se2.2: the remote edge is the toron daemon — a hex message id must be
    /// forwarded verbatim, never parsed. With a dead daemon the error is a
    /// transport failure; a "decimal"/"invalid digit" error would mean the
    /// id was coerced before it ever left (the F3 bug).
    #[test]
    fn remote_ack_passes_hex_id_through() {
        let cfg = MailConfig {
            url: Some("http://127.0.0.1:1".into()),
            token: None,
            name: Some("smoke".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        let err = mail
            .ack("proj", "smoke", "a1b2c3d4e5f60718293a4b5c6d7e8f90")
            .expect_err("hex id forwarded; dead daemon must still fail the call");
        let s = format!("{err:#}");
        assert!(!s.contains("decimal"), "no id parse before dial: {s}");
        assert!(!s.contains("invalid digit"), "no id parse before dial: {s}");
    }

    /// se2.2: mark_read's remote edge has the same pass-through honesty.
    #[test]
    fn remote_mark_read_passes_hex_id_through() {
        let cfg = MailConfig {
            url: Some("http://127.0.0.1:1".into()),
            token: None,
            name: Some("smoke".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        let err = mail
            .mark_read("proj", "smoke", "a1b2c3d4e5f60718293a4b5c6d7e8f90")
            .expect_err("hex id forwarded; dead daemon must still fail the call");
        let s = format!("{err:#}");
        assert!(!s.contains("decimal"), "no id parse before dial: {s}");
        assert!(!s.contains("invalid digit"), "no id parse before dial: {s}");
    }

    /// Live remote-daemon smoke: with a `toron serve --mcp-http` daemon up and
    /// `FLYWHEEL_AGENTMAIL_URL` set, exercises the facade's remote path
    /// end-to-end — `health` carries the `http` transport marker and
    /// `list_agents` returns `[]` from the daemon's fresh mailbox.
    #[test]
    #[ignore = "requires a live toron serve --mcp-http daemon (FLYWHEEL_AGENTMAIL_URL)"]
    fn remote_facade_against_live_daemon() {
        let Some(url) = std::env::var("FLYWHEEL_AGENTMAIL_URL")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("skipping: FLYWHEEL_AGENTMAIL_URL not set");
            return;
        };
        let cfg = MailConfig {
            url: Some(url),
            token: None,
            name: Some("smoke-test".into()),
        };
        let mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        let h = mail.health().expect("remote health");
        assert_eq!(h["transport"], json!("http"), "remote marker present");
        let agents = mail
            .list_agents("/tmp/flywheel-remote-smoke", None)
            .expect("remote list_agents");
        assert_eq!(agents, json!([]), "fresh daemon mailbox has no agents");
    }

    /// c3b.3: `urgent_only` + `since_ts` filter the toron inbox; the row
    /// model supports them (importance + created_ts_us). Seeded via real
    /// send_message round trips so the projection is the code under test.
    #[test]
    fn inbox_filtered_applies_urgent_and_since_on_toron() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("c3b3-filter-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        mail.register_agent(&project, "FilterSender", "opencode", "test", "seed")
            .expect("sender registered");
        mail.register_agent(&project, "FilterRecv", "opencode", "test", "seed")
            .expect("recipient registered");
        // The facade sends AS its own identity (`mail.name` = "flywheel") —
        // it must be registered too or send_message fails on the local key.
        mail.register_agent(&project, &mail.name, "opencode", "test", "seed")
            .expect("facade identity registered");
        let recv = vec!["FilterRecv".to_string()];

        let send = |subject: &str, importance: &str| {
            mail.send(&project, &recv, subject, "body", importance, false, None)
                .expect("send ok");
        };
        send("urgent-note", "urgent");
        send("high-note", "high");
        send("normal-note", "normal");
        send("low-note", "low");

        // urgent_only: only high/urgent rows survive.
        let urgent = mail
            .inbox_filtered(
                &project,
                "FilterRecv",
                &MailInboxFilter {
                    urgent_only: true,
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("filtered inbox");
        let subjects: Vec<&str> = urgent
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["subject"].as_str())
            .collect();
        assert!(
            subjects
                .iter()
                .all(|s| *s == "urgent-note" || *s == "high-note"),
            "urgent_only keeps only high/urgent: {subjects:?}"
        );
        assert!(subjects.contains(&"urgent-note"), "urgent present");
        assert!(subjects.contains(&"high-note"), "high present");

        // since_ts: only rows strictly newer than the bound survive.
        let since = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(3600))
            .expect("1h ago")
            .to_rfc3339();
        let recent = mail
            .inbox_filtered(
                &project,
                "FilterRecv",
                &MailInboxFilter {
                    since_ts: Some(since),
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("since-filtered inbox");
        let recent_subjects: Vec<&str> = recent
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["subject"].as_str())
            .collect();
        assert_eq!(
            recent_subjects.len(),
            4,
            "all four sent within the last hour: {recent_subjects:?}"
        );

        // since_ts in the future: nothing matches.
        let future = chrono::Utc::now()
            .checked_add_signed(chrono::Duration::seconds(3600))
            .expect("1h ahead")
            .to_rfc3339();
        let none = mail
            .inbox_filtered(
                &project,
                "FilterRecv",
                &MailInboxFilter {
                    since_ts: Some(future),
                    include_bodies: false,
                    limit: 100,
                    ..Default::default()
                },
            )
            .expect("future since-filtered inbox");
        assert_eq!(
            none.as_array().unwrap().len(),
            0,
            "nothing is newer than a future bound"
        );
    }

    /// c3b.3: `ack_overdue_only` has no toron projection (rows carry no
    /// ack_ts) — the request fails with an honest error naming the filter,
    /// never a silent unfiltered inbox.
    #[test]
    fn inbox_filtered_ack_overdue_is_honest_unsupported() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("c3b3-ackoverdue-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        let err = mail
            .inbox_filtered(
                &project,
                "Nobody",
                &MailInboxFilter {
                    ack_overdue_only: true,
                    include_bodies: false,
                    limit: 10,
                    ..Default::default()
                },
            )
            .expect_err("ack_overdue_only must fail loudly");
        let msg = err.to_string();
        assert!(
            msg.contains("ack_overdue_only") && msg.contains("ack_ts"),
            "error names the filter + the missing data: {msg}"
        );
    }

    /// c3b.3: a malformed `since_ts` is an error, never a silently-ignored
    /// filter.
    #[test]
    fn inbox_filtered_bad_since_ts_is_an_error() {
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let project = format!("c3b3-badsince-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        let err = mail
            .inbox_filtered(
                &project,
                "Nobody",
                &MailInboxFilter {
                    since_ts: Some("not-a-timestamp".into()),
                    include_bodies: false,
                    limit: 10,
                    ..Default::default()
                },
            )
            .expect_err("bad since_ts must fail loudly");
        assert!(
            err.to_string().contains("RFC3339"),
            "names the parse failure: {err}"
        );
    }

    /// c3b.7: herdr IS the active multiplexer (default) but the daemon is
    /// unreachable → the cleanup must return Err naming the outage, never an
    /// Ok note that reads like success.
    #[test]
    fn cleanup_pane_identities_herdr_down_is_err() {
        let _g = crate::config::lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
        // Point the herdr socket at a dead path so connect fails fast.
        unsafe { std::env::set_var("HERDR_SOCKET_PATH", "/tmp/flywheel-c3b7-dead.sock") };
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let err = mail
            .cleanup_pane_identities("c3b7-herdr-down")
            .expect_err("herdr down must be an error, not Ok(note)");
        let msg = err.to_string();
        assert!(
            msg.contains("herdr unreachable"),
            "names the outage + skip: {msg}"
        );
        unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };
    }

    /// c3b.7: the active multiplexer is NOT herdr (tmux) → the cleanup is
    /// genuinely not applicable → Ok with the not-wired note (a config fact,
    /// not a failure).
    #[test]
    fn cleanup_pane_identities_non_herdr_mux_is_ok_note() {
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "tmux") };
        let mail = Mail::from_config(None).expect("ok").expect("Some");
        let v = mail
            .cleanup_pane_identities("c3b7-not-herdr")
            .expect("non-herdr mux is not a failure");
        let note = v.get("note").and_then(Value::as_str).unwrap_or_default();
        assert!(
            note.contains("not herdr"),
            "note explains the config fact: {note}"
        );
        unsafe { std::env::remove_var("FLYWHEEL_BACKEND") };
    }
}

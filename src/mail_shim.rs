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
//! - the in-process mailbox is per-process by default: state does not survive
//!   separate flywheel invocations unless `TORON_DATA_DIR` (or the
//!   `TORON_MCP_DATA_DIR` alias) is set — the explicit durable root
//!   (se2.1 Phase A; Phase B makes `$XDG_DATA_HOME/toron` the default, gated
//!   on toron R2.0 KDF);
//! - toron message ids are hex event ids, not u64 sequence numbers;
//! - `ensure_product` needs an operator key; `demo_mailbox` has none.
//!
//! governed-by: ADR-0003

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use tokio::runtime::Runtime;

use toron_core::mailbox::{
    ContactPolicy, FetchInbox, ListAgentsOptions, RequestContact, RespondContact, SearchMessages,
    SendMessage,
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
/// every `ensure_project` on first use. Root dirs honor `TORON_DATA_DIR` /
/// `TORON_MCP_DATA_DIR` when set (se2.1 Phase A).
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

/// Root dirs for the in-process mailbox (se2.1 Phase A): `TORON_DATA_DIR`
/// (or the `TORON_MCP_DATA_DIR` alias) when set — explicit durable root for
/// tests/prod, with the SQLite index beside it. Default stays per-invocation
/// temp until Phase B flips it to `$XDG_DATA_HOME/toron` (gated on toron
/// R2.0 KDF). Returns (data_dir, sqlite db path).
fn mailbox_roots() -> (PathBuf, PathBuf) {
    for var in ["TORON_DATA_DIR", "TORON_MCP_DATA_DIR"] {
        if let Ok(dir) = std::env::var(var) {
            let dir = dir.trim();
            if !dir.is_empty() {
                let root = PathBuf::from(dir);
                return (root.clone(), root.join("flywheel-mail.sqlite"));
            }
        }
    }
    (
        std::env::temp_dir().join("flywheel-mail"),
        std::env::temp_dir().join(format!("flywheel-mail-{}.sqlite", std::process::id())),
    )
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
fn block_on<F: std::future::Future>(f: F) -> F::Output {
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
    /// Not yet applied by the toron projection (parity surface kept).
    #[allow(dead_code)]
    pub urgent_only: bool,
    pub unread_only: bool,
    /// Not yet applied by the toron projection (parity surface kept).
    #[allow(dead_code)]
    pub ack_overdue_only: bool,
    pub include_bodies: bool,
    pub limit: u32,
    /// Not yet applied by the toron projection (parity surface kept).
    #[allow(dead_code)]
    pub since_ts: Option<String>,
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
pub struct Mail {
    /// The agent identity this facade sends AS / reads the inbox for.
    pub name: String,
    /// Active backend (in-process default; remote when `url` is set).
    backend: BackendKind,
}

#[cfg_attr(not(feature = "mail-toron"), allow(dead_code))]
impl Mail {
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
    pub fn health(&mut self) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let report = block_on(mailbox().health_check());
                Ok(report.to_json())
            }
            BackendKind::Remote(_) => {
                // `project_key` is unused by the toron tool; empty string.
                let mut v = self.call_remote("health_check", json!({ "project_key": "" }))?;
                v["transport"] = json!("http"); // distinguishing marker: remote mode
                Ok(v)
            }
        }
    }

    /// `ensure_project`.
    pub fn ensure_project(&mut self, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let state = block_on(mailbox().ensure_project(project_key))?;
                Ok(state.to_json())
            }
            BackendKind::Remote(_) => {
                self.call_remote("ensure_project", json!({ "human_key": project_key }))
            }
        }
    }

    /// `list_agents` (project's registered identities).
    pub fn list_agents(&mut self, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let agents =
                    block_on(mailbox().list_agents(project_key, ListAgentsOptions::default()))?;
                Ok(Value::Array(agents.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => {
                self.call_remote("list_agents", json!({ "project_key": project_key }))
            }
        }
    }

    /// `register_agent` (upsert on the agent's key).
    pub fn register_agent(
        &mut self,
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
                json!({
                    "project_key": project_key,
                    "name": name,
                    "program": program,
                    "model": model,
                    "task_description": task_description,
                }),
            ),
        }
    }

    /// `fetch_inbox` (simple filter set).
    pub fn inbox(
        &mut self,
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
                }))?;
                Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "fetch_inbox",
                json!({
                    "project_key": project_key,
                    "agent_name": agent_name,
                    "limit": limit,
                    "include_bodies": true,
                }),
            ),
        }
    }

    /// `fetch_inbox` with the full filter bag. `urgent_only` /
    /// `ack_overdue_only` / `since_ts` have no toron projection yet — the
    /// supported subset (`unread_only`, `include_bodies`, `limit`) is applied.
    pub fn inbox_filtered(
        &mut self,
        project_key: &str,
        agent_name: &str,
        filter: &MailInboxFilter,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: filter.unread_only,
                    limit: Some(filter.limit as usize),
                    include_bodies: filter.include_bodies,
                }))?;
                Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "fetch_inbox",
                json!({
                    "project_key": project_key,
                    "agent_name": agent_name,
                    "limit": filter.limit,
                    "include_bodies": filter.include_bodies,
                    "unread_only": filter.unread_only,
                }),
            ),
        }
    }

    /// `mark_message_read`.
    pub fn mark_read(
        &mut self,
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
                    json!({
                        "project_key": project_key,
                        "agent_name": agent_name,
                        "message_id": message_id,
                    }),
                )
            }
        }
    }

    /// `acknowledge_message`.
    pub fn ack(&mut self, project_key: &str, agent_name: &str, message_id: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                // toron ids are hex event ids — pass through verbatim.
                let r = block_on(mailbox().acknowledge_message(project_key, agent_name, message_id))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => {
                // Same as mark_message_read above: the toron daemon's
                // `acknowledge_message` takes the hex event id as a String —
                // pass it through verbatim (se2.2).
                self.call_remote(
                    "acknowledge_message",
                    json!({
                        "project_key": project_key,
                        "agent_name": agent_name,
                        "message_id": message_id,
                    }),
                )
            }
        }
    }

    /// Messages past the ack SLA. toron has no ack-overdue projection yet —
    /// returns the inbox (empty after all are acked), so the escalation
    /// protocol's "nothing overdue" branch behaves as before.
    #[allow(dead_code)] // consumed by the Phase-4 escalation protocol + live test
    pub fn ack_overdue(&mut self, project_key: &str, agent_name: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
                    project_slug: project_key.to_string(),
                    agent_name: agent_name.to_string(),
                    unread_only: false,
                    limit: None,
                    include_bodies: false,
                }))?;
                Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "fetch_inbox",
                json!({
                    "project_key": project_key,
                    "agent_name": agent_name,
                    "include_bodies": false,
                }),
            ),
        }
    }

    /// `send_message`.
    #[allow(clippy::too_many_arguments)] // mirrors the MailBackend::send trait surface
    pub fn send(
        &mut self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack: bool,
        thread_id: Option<&str>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let result = block_on(mailbox().send_message(&SendMessage {
                    project_slug: project_key.to_string(),
                    sender_name: self.name.clone(),
                    to: to.to_vec(),
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
                let mut args = json!({
                    "project_key": project_key,
                    "sender_name": self.name,
                    "to": to,
                    "subject": subject,
                    "body_md": body,
                    "importance": importance,
                    "ack_required": ack,
                });
                if let Some(tid) = thread_id {
                    args["thread_id"] = json!(tid);
                }
                self.call_remote("send_message", args)
            }
        }
    }

    /// `reply_message` (threaded).
    pub fn reply(
        &mut self,
        project_key: &str,
        message_id: &str,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                // toron: the id IS the thread anchor (hex event id or the
                // free-form thread id — both flow through the `thread` tag).
                let result = block_on(mailbox().reply_message(&SendMessage {
                    project_slug: project_key.to_string(),
                    sender_name: self.name.clone(),
                    to: to.map(|t| t.to_vec()).unwrap_or_default(),
                    subject: String::new(),
                    body_md: body.to_string(),
                    thread_id: Some(message_id.to_string()),
                    ..Default::default()
                }))?;
                Ok(result.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "reply_message",
                json!({
                    "project_key": project_key,
                    "sender_name": self.name,
                    "to": to.map(|t| t.to_vec()).unwrap_or_default(),
                    "subject": "",
                    "body_md": body,
                    "thread_id": message_id.to_string(),
                }),
            ),
        }
    }

    /// `summarize_thread`.
    pub fn summarize_thread(
        &mut self,
        project_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let s = block_on(mailbox().summarize_thread(
                    project_key,
                    thread_id,
                    false,
                    llm_mode,
                    None,
                ))?;
                Ok(s.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "summarize_thread",
                json!({
                    "project_key": project_key,
                    "thread_id": thread_id,
                    "llm_mode": llm_mode,
                }),
            ),
        }
    }

    /// `summarize_thread_product` — summarize a thread across a product's
    /// member projects (ADR-0003 V6 surface restore).
    pub fn summarize_thread_product(
        &mut self,
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
                json!({
                    "product_key": product_key,
                    "thread_id": thread_id,
                    "llm_mode": llm_mode,
                }),
            ),
        }
    }

    /// `search_messages`. The JSON envelope is `{"result": [...]}` so
    /// `harvest_latest` (debate / plan-review) reads `result[0].body_md`.
    pub fn search(
        &mut self,
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
                let mut args = json!({
                    "project_key": project_key,
                    "query": query,
                    "limit": limit,
                });
                if filter.include_body_md {
                    args["include_body_md"] = json!(true);
                }
                if let Some(s) = &filter.sender {
                    args["sender"] = json!(s);
                }
                if let Some(i) = &filter.importance {
                    args["importance"] = json!(i);
                }
                if let Some(t) = &filter.thread_id {
                    args["thread_id"] = json!(t);
                }
                if let Some(r) = &filter.ranking {
                    args["ranking"] = json!(r);
                }
                let res = self.call_remote("search_messages", args)?;
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
        let mut args = json!({
            "project_key": project_key, "query": query, "limit": limit
        });
        if filter.include_body_md {
            args["include_body_md"] = json!(true);
        }
        if let Some(s) = &filter.sender {
            args["sender"] = json!(s);
        }
        if let Some(i) = &filter.importance {
            args["importance"] = json!(i);
        }
        if let Some(t) = &filter.thread_id {
            args["thread_id"] = json!(t);
        }
        if let Some(d) = &filter.date_start {
            args["date_start"] = json!(d);
        }
        if let Some(d) = &filter.date_end {
            args["date_end"] = json!(d);
        }
        if let Some(r) = &filter.ranking {
            args["ranking"] = json!(r);
        }
        args
    }

    /// `file_reservation_paths`.
    pub fn reserve(
        &mut self,
        project_key: &str,
        paths: &[String],
        ttl_seconds: u64,
        exclusive: bool,
        reason: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().file_reservation_paths(
                    project_key,
                    &self.name,
                    paths,
                    Some(ttl_seconds),
                    exclusive,
                    Some(reason),
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "file_reservation_paths",
                json!({
                    "project_key": project_key,
                    "agent_name": self.name,
                    "paths": paths,
                    "ttl_seconds": ttl_seconds as i64,
                    "exclusive": exclusive,
                    "reason": reason,
                }),
            ),
        }
    }

    /// `release_file_reservations`.
    pub fn release(&mut self, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r =
                    block_on(mailbox().release_file_reservations(project_key, &self.name, &[]))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "release_file_reservations",
                json!({ "project_key": project_key, "agent_name": self.name }),
            ),
        }
    }

    /// `check_file_reservation_conflicts`.
    pub fn conflicts(&mut self, project_key: &str, paths: &[String]) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let r = block_on(mailbox().check_file_reservation_conflicts(
                    project_key,
                    &self.name,
                    paths,
                ))?;
                Ok(r.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "check_file_reservation_conflicts",
                json!({
                    "project_key": project_key,
                    "agent_name": self.name,
                    "paths": paths,
                }),
            ),
        }
    }

    /// `renew_file_reservations`. The response keeps the `file_reservations`
    /// envelope `list_own_reservations` parses (ids are toron hex event ids).
    pub fn renew(
        &mut self,
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
                        "old_expires_ts": "",
                        "new_expires_ts": res.expires_ts_us.map(|u| u.to_string()).unwrap_or_default(),
                    })).collect::<Vec<Value>>(),
                }))
            }
            BackendKind::Remote(_) => {
                let mut args = json!({
                    "project_key": project_key,
                    "agent_name": agent_name,
                    "extend_seconds": extend_seconds as i64,
                });
                if let Some(p) = paths {
                    args["paths"] = json!(p);
                }
                let res = self.call_remote("renew_file_reservations", args)?;
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
    /// reservation (ADR-0003 V6 surface restore). In-process requires the
    /// operator key (attached by `build_mailbox`); remote delegates to the
    /// daemon's tool.
    pub fn force_release_file_reservation(
        &mut self,
        project_key: &str,
        agent_name: &str,
        file_reservation_id: &str,
        note: Option<&str>,
        notify_previous: bool,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                block_on(mailbox().force_release_file_reservation(
                    project_key,
                    agent_name,
                    file_reservation_id,
                    note,
                    notify_previous,
                ))?;
                Ok(json!({ "ok": true }))
            }
            BackendKind::Remote(_) => {
                let mut args = json!({
                    "project_key": project_key,
                    "agent_name": agent_name,
                    "file_reservation_id": file_reservation_id,
                    "notify_previous": notify_previous,
                });
                if let Some(n) = note {
                    args["note"] = json!(n);
                }
                self.call_remote("force_release_file_reservation", args)
            }
        }
    }

    /// `cleanup_pane_identities` — prune identities whose panes are absent
    /// from herdr's live pane list (ADR-0003 V6 surface restore). toron holds
    /// no per-pane identity files (live herdr mapping), so in-process "prune"
    /// reports the stale names; without a herdr backend it returns the
    /// not-wired note (same contract as the toron-mcp tool).
    pub fn cleanup_pane_identities(&mut self, _project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                match toron_core::backend::herdr::HerdrBackend::connect(None) {
                    Ok(backend) => {
                        let pruned = mailbox().cleanup_pane_identities(&backend)?;
                        Ok(json!({ "pruned": pruned }))
                    }
                    Err(e) => Ok(json!({
                        "note": format!("pane cleanup requires a herdr backend; not wired: {e}"),
                    })),
                }
            }
            BackendKind::Remote(_) => self.call_remote(
                "cleanup_pane_identities",
                json!({ "project_key": _project_key }),
            ),
        }
    }

    // ---- cross-project contact + products (bd-mail-cross-project-contact) ----

    /// `request_contact`.
    pub fn request_contact(
        &mut self,
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
            BackendKind::Remote(_) => {
                let mut args = json!({
                    "project_key": project_key,
                    "from_agent": from_agent,
                    "to_agent": to_agent,
                    "reason": reason,
                });
                if let Some(tp) = to_project {
                    args["to_project"] = json!(tp);
                }
                self.call_remote("request_contact", args)
            }
        }
    }

    /// `respond_contact`.
    pub fn respond_contact(
        &mut self,
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
                json!({
                    "project_key": project_key,
                    "to_agent": to_agent,
                    "from_agent": from_agent,
                    "accept": accept,
                }),
            ),
        }
    }

    /// `list_contacts`.
    pub fn list_contacts(&mut self, project_key: &str, agent_name: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let links = block_on(mailbox().list_contacts(project_key, agent_name))?;
                Ok(Value::Array(links.iter().map(ToJson::to_json).collect()))
            }
            BackendKind::Remote(_) => self.call_remote(
                "list_contacts",
                json!({ "project_key": project_key, "agent_name": agent_name }),
            ),
        }
    }

    /// `set_contact_policy`.
    pub fn set_contact_policy(
        &mut self,
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
                    json!({
                        "project_key": project_key,
                        "agent_name": agent_name,
                        "policy": policy,
                    }),
                )?;
                Ok(json!({ "policy": policy, "event_id": res }))
            }
        }
    }

    /// `ensure_product`.
    pub fn ensure_product(&mut self, product_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let info = block_on(mailbox().ensure_product(product_key, product_key))?;
                Ok(info.to_json())
            }
            BackendKind::Remote(_) => {
                self.call_remote("ensure_product", json!({ "product_key": product_key }))
            }
        }
    }

    /// `products_link`.
    pub fn products_link(&mut self, product_key: &str, project_key: &str) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                let info = block_on(mailbox().products_link(product_key, project_key))?;
                Ok(info.to_json())
            }
            BackendKind::Remote(_) => self.call_remote(
                "products_link",
                json!({ "product_key": product_key, "project_key": project_key }),
            ),
        }
    }

    /// `search_messages_product`.
    pub fn search_product(&mut self, product_key: &str, query: &str, limit: u32) -> Result<Value> {
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
                    json!({
                        "product_key": product_key,
                        "query": query,
                        "limit": limit,
                        "include_body_md": true,
                    }),
                )?;
                Ok(json!({ "result": res }))
            }
        }
    }

    /// `fetch_inbox_product`.
    pub fn inbox_product(
        &mut self,
        product_key: &str,
        agent_name: &str,
        limit: u32,
    ) -> Result<Value> {
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
                    json!({
                        "product_key": product_key,
                        "agent_name": agent_name,
                        "limit": limit,
                    }),
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
        &mut self,
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
                json!({
                    "project_key": project_key,
                    "agent_name": agent_name,
                    "slot": slot,
                    "ttl_seconds": ttl_seconds as i64,
                    "exclusive": exclusive,
                }),
            ),
        }
    }

    /// `renew_build_slot` (ADR-0003 V6: the `MailBackend::slot_renew` surface
    /// consumes this; no longer dead code).
    pub fn renew_slot(
        &mut self,
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
                    json!({
                        "project_key": project_key,
                        "agent_name": agent_name,
                        "slot": slot,
                        "extend_seconds": extend_seconds as i64,
                    }),
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
    pub fn release_slot(
        &mut self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
    ) -> Result<Value> {
        match &self.backend {
            BackendKind::InProcess => {
                block_on(mailbox().release_build_slot(project_key, agent_name, slot))?;
                Ok(json!({ "slot": slot, "released": true }))
            }
            BackendKind::Remote(_) => {
                let res = self.call_remote(
                    "release_build_slot",
                    json!({
                        "project_key": project_key,
                        "agent_name": agent_name,
                        "slot": slot,
                    }),
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
    pub fn resolve_pane_identity(&mut self, project_key: &str, pane_id: &str) -> Result<Value> {
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

#[cfg(test)]
pub(crate) fn set_test_identity_base(path: Option<PathBuf>) {
    *TEST_IDENTITY_BASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
}

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
        assert!(RUNTIME.get().is_some(), "from_config initialized the runtime");
        assert!(block_on(mailbox().ensure_project("se2.1-runtime-test")).is_ok());
        assert!(
            block_on(mailbox().ensure_project("se2.1-runtime-test-2")).is_ok(),
            "second op reuses the same runtime"
        );
    }

    /// se2.1 Phase A: `TORON_DATA_DIR` (and the `TORON_MCP_DATA_DIR` alias)
    /// override the per-invocation temp default; unset/blank → temp.
    #[test]
    fn mailbox_roots_env_and_default() {
        unsafe { std::env::set_var("TORON_DATA_DIR", "/tmp/flywheel-se2.1-durable") };
        unsafe { std::env::set_var("TORON_MCP_DATA_DIR", "/tmp/flywheel-se2.1-mcp") };
        let (data_dir, db) = mailbox_roots();
        assert_eq!(data_dir, PathBuf::from("/tmp/flywheel-se2.1-durable"));
        assert_eq!(db, PathBuf::from("/tmp/flywheel-se2.1-durable/flywheel-mail.sqlite"));
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(data_dir, PathBuf::from("/tmp/flywheel-se2.1-mcp"), "alias fallback");
        unsafe { std::env::set_var("TORON_DATA_DIR", "   ") };
        let (data_dir, _) = mailbox_roots();
        assert_eq!(data_dir, PathBuf::from("/tmp/flywheel-se2.1-mcp"), "blank counts as unset");
        unsafe { std::env::remove_var("TORON_DATA_DIR") };
        unsafe { std::env::remove_var("TORON_MCP_DATA_DIR") };
        let (data_dir, db) = mailbox_roots();
        assert_eq!(data_dir, std::env::temp_dir().join("flywheel-mail"));
        assert_eq!(
            db,
            std::env::temp_dir().join(format!("flywheel-mail-{}.sqlite", std::process::id()))
        );
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
        let mut mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
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
        let mut mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
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
        let mut mail = Mail::from_config(Some(&cfg)).expect("ok").expect("Some");
        let h = mail.health().expect("remote health");
        assert_eq!(h["transport"], json!("http"), "remote marker present");
        let agents = mail
            .list_agents("/tmp/flywheel-remote-smoke")
            .expect("remote list_agents");
        assert_eq!(agents, json!([]), "fresh daemon mailbox has no agents");
    }
}

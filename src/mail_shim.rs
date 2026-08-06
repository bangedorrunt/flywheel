//! In-process agent-mail shim (ADR-0003 Decision 3A, bd-o8j.2).
//!
//! `src/mail.rs` (the HTTP JSON-RPC client to the agent-mail MCP server) was
//! deleted. This shim keeps the `Mail` surface call sites depend on (method
//! signatures + JSON shapes) but routes every operation through the IN-PROCESS
//! `toron_core::mailbox::Mailbox` (InProcessBus + SQLite index, an operator
//! key attached so `ensure_project`/send/register work) — zero HTTP, one
//! mailbox per process (lazily shared across `Mail` instances).
//!
//! flywheel main.rs dispatch is sync, so each op is `block_on`'d on a shared
//! tokio runtime. The pane-identity bridge helpers (`project_hash`,
//! `write_pane_identity`, …) are filesystem ops, not HTTP — kept verbatim.
//!
//! Known limitations of the demo mailbox (reported, not papered over):
//! - the mailbox is per-process: state does not survive separate flywheel
//!   invocations (the durable loop's leaf subprocesses each see a fresh one);
//! - toron message ids are hex event ids, not u64 sequence numbers;
//! - `ensure_product` needs an operator key; `demo_mailbox` has none.
//!
//! governed-by: ADR-0003

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
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

/// In-process mailbox context: the single shared mailbox every `Mail` instance
/// operates on. Mirrors `toron_mcp::demo_mailbox` (InProcessBus + SQLite index
/// in the system temp dir) but WITH an operator key, so write paths
/// (`ensure_project`, send, register) work — the demo mailbox is operator-less
/// and would reject every `ensure_project` on first use.
fn mailbox() -> &'static toron_core::mailbox::Mailbox {
    static MB: OnceLock<toron_core::mailbox::Mailbox> = OnceLock::new();
    MB.get_or_init(|| {
        let rt = Runtime::new().expect("flywheel mail: tokio runtime");
        rt.block_on(build_mailbox())
            .expect("flywheel mail: toron in-process mailbox")
    })
}

async fn build_mailbox() -> anyhow::Result<toron_core::mailbox::Mailbox> {
    let db = std::env::temp_dir().join(format!("flywheel-mail-{}.sqlite", std::process::id()));
    let pool = toron_core::index::schema::connect(db.to_str().unwrap()).await?;
    toron_core::index::schema::migrate(&pool).await?;
    let bus = toron_core::bus::InProcessBus::open(
        pool.clone(),
        toron_core::bus::in_process::InProcessBusConfig::default(),
    )
    .await?;
    let data_dir = std::env::temp_dir().join("flywheel-mail");
    let operator = toron_core::keys::generate();
    Ok(toron_core::mailbox::Mailbox::new(
        std::sync::Arc::new(bus),
        data_dir,
        Some(operator),
    )
    .with_index_pool(pool))
}

/// Run one async mailbox operation on a fresh tokio runtime (flywheel main.rs
/// dispatch is sync; each op is a short-lived, sequential block_on).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    Runtime::new()
        .expect("flywheel mail: tokio runtime")
        .block_on(f)
}

/// Full inbox filter bag (bd-mail-ack-sla-tracking). Callers construct this
/// instead of passing 5 booleans positionally.
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

/// In-process agent-mail facade. Every method maps 1:1 onto a
/// `toron_core::mailbox::Mailbox` call (see the deleted `src/mail.rs` for the
/// surface this replaces).
pub struct Mail {
    /// The agent identity this facade sends AS / reads the inbox for.
    pub name: String,
}

impl Mail {
    /// Construct the in-process mail facade. `[agent_mail]` config is now
    /// OPTIONAL: the integrated mailbox is always available (only the agent
    /// identity name is read, defaulting to `flywheel`). There is no
    /// remote-daemon HTTP fallback.
    pub fn in_process(cfg: Option<&MailConfig>) -> Result<Option<Self>> {
        let name = cfg
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| "flywheel".to_string());
        Ok(Some(Mail { name }))
    }

    /// `health_check` (in-process).
    pub fn health(&mut self) -> Result<Value> {
        let report = block_on(mailbox().health_check());
        Ok(report.to_json())
    }

    /// `ensure_project`.
    pub fn ensure_project(&mut self, project_key: &str) -> Result<Value> {
        let state = block_on(mailbox().ensure_project(project_key))?;
        Ok(state.to_json())
    }

    /// `list_agents` (project's registered identities).
    pub fn list_agents(&mut self, project_key: &str) -> Result<Value> {
        let agents = block_on(mailbox().list_agents(project_key, ListAgentsOptions::default()))?;
        Ok(Value::Array(agents.iter().map(ToJson::to_json).collect()))
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
        let mut profile = AgentProfile::new(name);
        profile.program = Some(program.to_string());
        profile.model = Some(model.to_string());
        profile.task_description = Some(task_description.to_string());
        let agent = block_on(mailbox().register_agent(project_key, &profile))?;
        Ok(agent.to_json())
    }

    /// `fetch_inbox` (simple filter set).
    pub fn inbox(
        &mut self,
        project_key: &str,
        agent_name: &str,
        _urgent_only: bool,
        limit: u32,
    ) -> Result<Value> {
        let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
            project_slug: project_key.to_string(),
            agent_name: agent_name.to_string(),
            unread_only: false,
            limit: Some(limit as usize),
            include_bodies: true,
        }))?;
        Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
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
        let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
            project_slug: project_key.to_string(),
            agent_name: agent_name.to_string(),
            unread_only: filter.unread_only,
            limit: Some(filter.limit as usize),
            include_bodies: filter.include_bodies,
        }))?;
        Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
    }

    /// `mark_message_read`.
    pub fn mark_read(
        &mut self,
        project_key: &str,
        agent_name: &str,
        message_id: u64,
    ) -> Result<Value> {
        let r = block_on(mailbox().mark_message_read(
            project_key,
            agent_name,
            &message_id.to_string(),
        ))?;
        Ok(r.to_json())
    }

    /// `acknowledge_message`.
    pub fn ack(&mut self, project_key: &str, agent_name: &str, message_id: u64) -> Result<Value> {
        let r = block_on(mailbox().acknowledge_message(
            project_key,
            agent_name,
            &message_id.to_string(),
        ))?;
        Ok(r.to_json())
    }

    /// Messages past the ack SLA. toron has no ack-overdue projection yet —
    /// returns the inbox (empty after all are acked), so the escalation
    /// protocol's "nothing overdue" branch behaves as before.
    #[allow(dead_code)] // consumed by the Phase-4 escalation protocol + live test
    pub fn ack_overdue(&mut self, project_key: &str, agent_name: &str) -> Result<Value> {
        let msgs = block_on(mailbox().fetch_inbox(&FetchInbox {
            project_slug: project_key.to_string(),
            agent_name: agent_name.to_string(),
            unread_only: false,
            limit: None,
            include_bodies: false,
        }))?;
        Ok(Value::Array(msgs.iter().map(ToJson::to_json).collect()))
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

    /// `reply_message` (threaded).
    pub fn reply(
        &mut self,
        project_key: &str,
        message_id: u64,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
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

    /// `summarize_thread`.
    pub fn summarize_thread(
        &mut self,
        project_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        let s = block_on(mailbox().summarize_thread(project_key, thread_id, false, llm_mode, None))?;
        Ok(s.to_json())
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

    /// `release_file_reservations`.
    pub fn release(&mut self, project_key: &str) -> Result<Value> {
        let r = block_on(mailbox().release_file_reservations(project_key, &self.name, &[]))?;
        Ok(r.to_json())
    }

    /// `check_file_reservation_conflicts`.
    pub fn conflicts(&mut self, project_key: &str, paths: &[String]) -> Result<Value> {
        let r = block_on(mailbox().check_file_reservation_conflicts(project_key, &self.name, paths))?;
        Ok(r.to_json())
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
        let link = block_on(mailbox().request_contact(&RequestContact {
            project_slug: project_key.to_string(),
            from_agent: from_agent.to_string(),
            to_agent: to_agent.to_string(),
            to_project: to_project.map(String::from),
            reason: Some(reason.to_string()),
        }))?;
        Ok(link.to_json())
    }

    /// `respond_contact`.
    pub fn respond_contact(
        &mut self,
        project_key: &str,
        to_agent: &str,
        from_agent: &str,
        accept: bool,
    ) -> Result<Value> {
        let link = block_on(mailbox().respond_contact(&RespondContact {
            project_slug: project_key.to_string(),
            to_agent: to_agent.to_string(),
            from_agent: from_agent.to_string(),
            accept,
            from_project: None,
        }))?;
        Ok(link.to_json())
    }

    /// `list_contacts`.
    pub fn list_contacts(&mut self, project_key: &str, agent_name: &str) -> Result<Value> {
        let links = block_on(mailbox().list_contacts(project_key, agent_name))?;
        Ok(Value::Array(links.iter().map(ToJson::to_json).collect()))
    }

    /// `set_contact_policy`.
    pub fn set_contact_policy(
        &mut self,
        project_key: &str,
        agent_name: &str,
        policy: &str,
    ) -> Result<Value> {
        let id = block_on(mailbox().set_contact_policy(
            project_key,
            agent_name,
            ContactPolicy::parse(policy),
        ))?;
        Ok(json!({ "policy": policy, "event_id": id }))
    }

    /// `ensure_product`.
    pub fn ensure_product(&mut self, product_key: &str) -> Result<Value> {
        let info = block_on(mailbox().ensure_product(product_key, product_key))?;
        Ok(info.to_json())
    }

    /// `products_link`.
    pub fn products_link(&mut self, product_key: &str, project_key: &str) -> Result<Value> {
        let info = block_on(mailbox().products_link(product_key, project_key))?;
        Ok(info.to_json())
    }

    /// `search_messages_product`.
    pub fn search_product(
        &mut self,
        product_key: &str,
        query: &str,
        limit: u32,
    ) -> Result<Value> {
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

    /// `fetch_inbox_product`.
    pub fn inbox_product(&mut self, product_key: &str, agent_name: &str, limit: u32) -> Result<Value> {
        let res = block_on(mailbox().fetch_inbox_product(product_key, agent_name))?;
        Ok(json!({ "messages": res.iter().take(limit as usize).map(|(p, m)| {
            let mut v = m.to_json();
            v["project"] = json!(p);
            v
        }).collect::<Vec<Value>>() }))
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
        let r = block_on(mailbox().acquire_build_slot(
            project_key,
            agent_name,
            slot,
            Some(ttl_seconds),
            exclusive,
        ))?;
        Ok(r.to_json())
    }

    /// `renew_build_slot`.
    #[allow(dead_code)] // parity surface; the MailBackend trait has no slot_renew
    pub fn renew_slot(
        &mut self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
        extend_seconds: u64,
    ) -> Result<Value> {
        let r = block_on(mailbox().renew_build_slot(project_key, agent_name, slot, extend_seconds))?;
        Ok(match r {
            Some(b) => b.to_json(),
            None => json!({ "slot": slot, "renewed": false }),
        })
    }

    /// `release_build_slot`.
    pub fn release_slot(&mut self, project_key: &str, agent_name: &str, slot: &str) -> Result<Value> {
        block_on(mailbox().release_build_slot(project_key, agent_name, slot))?;
        Ok(json!({ "slot": slot, "released": true }))
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

    /// Remove the per-pane identity file (pane exited / respawned). No-op if
    /// absent — duplicate exit events must not fail. Returns the path.
    pub fn clear_pane_identity(&self, project_key: &str, pane_id: &str) -> Result<PathBuf> {
        let path = canonical_identity_path(project_key, pane_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(e) => Err(e).context("remove pane identity"),
        }
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
fn identity_base() -> PathBuf {
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
fn write_identity_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename onto {}", path.display()))?;
    Ok(())
}

/// One renewed reservation, as reported by `renew_file_reservations`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RenewedReservation {
    pub id: u64,
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
            id: r.get("id").and_then(Value::as_u64).unwrap_or(0),
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

//! agent-mail coordination client — JSON-RPC over HTTP against the MCP endpoint.
//! Server: http://127.0.0.1:8765/mcp/ (bearer token from config). Tools return
//! JSON-serialized text content; we deserialize the inner JSON.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::MailConfig;

/// Bound every HTTP call — a hanging agent-mail server must not stall the
/// loop (the durable `mail` path blocks on this client).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

fn post(url: &str, token: Option<&str>, body: Value) -> Result<Value> {
    let mut req = ureq::post(url)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/json");
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = req
        .send_json(body)
        .map_err(|e| anyhow!("agent-mail request failed: {e}"))?;
    let out: Value = resp.into_json().context("agent-mail returned non-JSON")?;
    if let Some(err) = out.get("error") {
        bail!(
            "agent-mail error: {}",
            err.get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    Ok(out)
}

/// Extract the JSON value inside an MCP tool result's first text content.
fn tool_result(resp: Value) -> Result<Value> {
    let content = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("agent-mail response missing content: {resp}"))?;
    for c in content {
        if let Some(text) = c.get("text").and_then(Value::as_str) {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            // tool output is JSON text; fall back to raw string if not JSON
            return Ok(serde_json::from_str(trimmed).unwrap_or_else(|_| json!(trimmed)));
        }
    }
    bail!("agent-mail response has no text content")
}

pub struct Mail {
    pub base: String,
    pub token: Option<String>,
    pub name: String,
    next_id: u64,
}

/// Full inbox filter bag (bd-mail-ack-sla-tracking). Callers construct this
/// instead of passing 5 booleans positionally.
#[derive(Debug, Clone, Default)]
pub struct MailInboxFilter {
    pub urgent_only: bool,
    pub unread_only: bool,
    pub ack_overdue_only: bool,
    pub include_bodies: bool,
    pub limit: u32,
    pub since_ts: Option<String>,
}

impl Mail {
    pub fn from_config(cfg: Option<&MailConfig>) -> Result<Option<Self>> {
        let Some(c) = cfg else { return Ok(None) };
        let url = c
            .url
            .clone()
            .ok_or_else(|| anyhow!("agent_mail.url unset"))?;
        let name = c.name.clone().unwrap_or_else(|| "flywheel".to_string());
        Ok(Some(Mail {
            base: url,
            token: c.token.clone(),
            name,
            next_id: 1,
        }))
    }

    fn call(&mut self, tool: &str, args: Value) -> Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let body = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        });
        let resp = post(
            &format!("{}/mcp/", self.base.trim_end_matches('/')),
            self.token.as_deref(),
            body,
        )?;
        tool_result(resp)
    }

    /// GET /health on the server base (not an MCP tool).
    pub fn health(&mut self) -> Result<Value> {
        let url = format!("{}/health", self.base.trim_end_matches('/'));
        let mut req = ureq::get(&url).timeout(HTTP_TIMEOUT);
        if let Some(t) = self.token.as_deref() {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = req
            .call()
            .map_err(|e| anyhow!("agent-mail health request failed: {e}"))?;
        resp.into_json()
            .context("agent-mail /health returned non-JSON")
    }

    pub fn ensure_project(&mut self, project_key: &str) -> Result<Value> {
        self.call("ensure_project", json!({ "human_key": project_key }))
    }

    pub fn list_agents(&mut self, project_key: &str) -> Result<Value> {
        self.call("list_agents", json!({ "project_key": project_key }))
    }

    /// Register or update an agent identity in a project (agent-mail MCP
    /// `register_agent` tool). Idempotent on name — re-register updates the
    /// profile + refreshes `last_active_ts`. Wraps the tool so `cmd_spawn`
    /// can register each spawned agent, making it addressable by name in mail.
    pub fn register_agent(
        &mut self,
        project_key: &str,
        name: &str,
        program: &str,
        model: &str,
        task_description: &str,
    ) -> Result<Value> {
        self.call(
            "register_agent",
            json!({
                "project_key": project_key,
                "name": name,
                "program": program,
                "model": model,
                "task_description": task_description,
            }),
        )
    }

    pub fn inbox(
        &mut self,
        project_key: &str,
        agent_name: &str,
        urgent_only: bool,
        limit: u32,
    ) -> Result<Value> {
        self.call(
            "fetch_inbox",
            json!({
                "project_key": project_key, "agent_name": agent_name,
                "urgent_only": urgent_only, "limit": limit
            }),
        )
    }

    /// Full inbox filter bag (bd-mail-ack-sla-tracking). Callers construct
    /// this instead of passing 5 booleans positionally.
    /// Fetch the inbox with the full agent-mail filter set
    /// (`fetch_inbox`): `unread_only`, `ack_overdue_only`, `since_ts`,
    /// `include_bodies`, `limit` — plus `urgent_only` for parity with the
    /// simple `inbox`.
    pub fn inbox_filtered(
        &mut self,
        project_key: &str,
        agent_name: &str,
        filter: &MailInboxFilter,
    ) -> Result<Value> {
        let mut args = json!({
            "project_key": project_key, "agent_name": agent_name,
            "urgent_only": filter.urgent_only,
            "unread_only": filter.unread_only,
            "ack_overdue_only": filter.ack_overdue_only,
            "include_bodies": filter.include_bodies,
            "limit": filter.limit,
        });
        if let Some(ts) = &filter.since_ts {
            args["since_ts"] = json!(ts);
        }
        self.call("fetch_inbox", args)
    }

    /// Mark a message read (`mark_message_read`): "I've seen it" — distinct
    /// from ack, which means "I've acted on it".
    pub fn mark_read(
        &mut self,
        project_key: &str,
        agent_name: &str,
        message_id: u64,
    ) -> Result<Value> {
        self.call(
            "mark_message_read",
            json!({
                "project_key": project_key, "agent_name": agent_name,
                "message_id": message_id
            }),
        )
    }

    /// Acknowledge a message (`acknowledge_message`): sets both `read_ts`
    /// and `ack_ts`. The ack contract: agents call this after ACTING on an
    /// `ack_required=true` message — the orchestrator cannot ack for them
    /// (ack is per-recipient).
    pub fn ack(&mut self, project_key: &str, agent_name: &str, message_id: u64) -> Result<Value> {
        self.call(
            "acknowledge_message",
            json!({
                "project_key": project_key, "agent_name": agent_name,
                "message_id": message_id
            }),
        )
    }

    /// Messages past the ack SLA (agent-mail's `ack_overdue_only` — default
    /// 30-min window). The escalation protocol polls this to find exactly
    /// which peers ignored their pings.
    #[allow(dead_code)] // consumed by the Phase-4 escalation protocol + live test
    pub fn ack_overdue(&mut self, project_key: &str, agent_name: &str) -> Result<Value> {
        self.call(
            "fetch_inbox",
            json!({
                "project_key": project_key, "agent_name": agent_name,
                "ack_overdue_only": true
            }),
        )
    }

    /// Send a message. `thread_id` chains this message into an existing
    /// thread or starts one (AD-0001 A3, bd-mail-reply-thread). flywheel's
    /// thread-id conventions: bead work `<bead-id>`, debate `debate-<ts>`,
    /// plan review `plan-<bead-id>` / `plan-<ts>`, health `health-<agent>`.
    /// `None` = standalone message (never guess a thread — explicit only).
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
        let mut args = json!({
            "project_key": project_key, "sender_name": self.name, "to": to,
            "subject": subject, "body_md": body, "importance": importance,
            "ack_required": ack
        });
        if let Some(t) = thread_id {
            args["thread_id"] = json!(t);
        }
        self.call("send_message", args)
    }

    /// Reply to a message (agent-mail MCP `reply_message` tool). Omitting
    /// `to` replies to the original sender (the tool contract); the reply
    /// inherits the original's thread, importance and ack flags unless the
    /// tool's defaults are overridden.
    pub fn reply(
        &mut self,
        project_key: &str,
        message_id: u64,
        body: &str,
        to: Option<&[String]>,
    ) -> Result<Value> {
        let mut args = json!({
            "project_key": project_key, "sender_name": self.name,
            "message_id": message_id, "body_md": body
        });
        if let Some(to) = to {
            args["to"] = json!(to);
        }
        self.call("reply_message", args)
    }

    /// Summarize a thread (agent-mail MCP `summarize_thread` tool).
    /// `llm_mode=false` (default) uses structured extraction — deterministic,
    /// what the orchestrator/judge needs to read a thread compactly;
    /// `llm_mode=true` produces a human-readable digest.
    pub fn summarize_thread(
        &mut self,
        project_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        self.call(
            "summarize_thread",
            json!({
                "project_key": project_key, "thread_id": thread_id,
                "llm_mode": llm_mode
            }),
        )
    }

    /// Search messages (agent-mail MCP `search_messages`, FTS5 over
    /// subject+body). Filters: sender, importance, thread_id, date range,
    /// ranking (relevance|recency). Used by the loop to answer "did a peer
    /// already solve this bead?" before re-dispatching
    /// (bd-mail-search-summarize). `limit` bounds the result set.
    pub fn search(
        &mut self,
        project_key: &str,
        query: &str,
        limit: u32,
        sender: Option<&str>,
        importance: Option<&str>,
        thread_id: Option<&str>,
        date_start: Option<&str>,
        date_end: Option<&str>,
        ranking: Option<&str>,
        include_body_md: bool,
    ) -> Result<Value> {
        self.call(
            "search_messages",
            Self::search_args(
                project_key,
                query,
                limit,
                sender,
                importance,
                thread_id,
                date_start,
                date_end,
                ranking,
                include_body_md,
            ),
        )
    }

    /// Build the `search_messages` args JSON (pure; unit-testable).
    /// review-fix F1: emits `include_body_md: true` when requested.
    pub fn search_args(
        project_key: &str,
        query: &str,
        limit: u32,
        sender: Option<&str>,
        importance: Option<&str>,
        thread_id: Option<&str>,
        date_start: Option<&str>,
        date_end: Option<&str>,
        ranking: Option<&str>,
        include_body_md: bool,
    ) -> Value {
        let mut args = json!({
            "project_key": project_key, "query": query, "limit": limit
        });
        if include_body_md {
            args["include_body_md"] = json!(true);
        }
        if let Some(s) = sender {
            args["sender"] = json!(s);
        }
        if let Some(i) = importance {
            args["importance"] = json!(i);
        }
        if let Some(t) = thread_id {
            args["thread_id"] = json!(t);
        }
        if let Some(d) = date_start {
            args["date_start"] = json!(d);
        }
        if let Some(d) = date_end {
            args["date_end"] = json!(d);
        }
        if let Some(r) = ranking {
            args["ranking"] = json!(r);
        }
        args
    }

    pub fn reserve(
        &mut self,
        project_key: &str,
        paths: &[String],
        ttl_seconds: u64,
        exclusive: bool,
        reason: &str,
    ) -> Result<Value> {
        self.call(
            "file_reservation_paths",
            json!({
                "project_key": project_key, "agent_name": self.name, "paths": paths,
                "ttl_seconds": ttl_seconds, "exclusive": exclusive, "reason": reason
            }),
        )
    }

    pub fn release(&mut self, project_key: &str) -> Result<Value> {
        self.call(
            "release_file_reservations",
            json!({ "project_key": project_key, "agent_name": self.name }),
        )
    }

    pub fn conflicts(&mut self, project_key: &str, paths: &[String]) -> Result<Value> {
        self.call(
            "check_file_reservation_conflicts",
            json!({ "project_key": project_key, "agent_name": self.name, "paths": paths }),
        )
    }

    // ---- cross-project contact + products (bd-mail-cross-project-contact) ----

    /// Request contact approval to message another agent (optionally cross-project).
    pub fn request_contact(
        &mut self,
        project_key: &str,
        from_agent: &str,
        to_agent: &str,
        to_project: Option<&str>,
        reason: &str,
        ttl_seconds: Option<u64>,
    ) -> Result<Value> {
        let mut args = json!({
            "project_key": project_key,
            "from_agent": from_agent,
            "to_agent": to_agent,
            "reason": reason,
        });
        if let Some(p) = to_project {
            args["to_project"] = json!(p);
        }
        if let Some(t) = ttl_seconds {
            args["ttl_seconds"] = json!(t);
        }
        self.call("request_contact", args)
    }

    /// Accept or deny a contact request.
    pub fn respond_contact(
        &mut self,
        project_key: &str,
        to_agent: &str,
        from_agent: &str,
        accept: bool,
    ) -> Result<Value> {
        self.call(
            "respond_contact",
            json!({
                "project_key": project_key,
                "to_agent": to_agent,
                "from_agent": from_agent,
                "accept": accept,
            }),
        )
    }

    /// List contact links for an agent.
    pub fn list_contacts(&mut self, project_key: &str, agent_name: &str) -> Result<Value> {
        self.call(
            "list_contacts",
            json!({ "project_key": project_key, "agent_name": agent_name }),
        )
    }

    /// Set contact policy: open | auto | contacts_only | block_all.
    pub fn set_contact_policy(
        &mut self,
        project_key: &str,
        agent_name: &str,
        policy: &str,
    ) -> Result<Value> {
        self.call(
            "set_contact_policy",
            json!({
                "project_key": project_key,
                "agent_name": agent_name,
                "policy": policy,
            }),
        )
    }

    /// Ensure a product exists (organizational group of projects).
    pub fn ensure_product(&mut self, product_key: &str) -> Result<Value> {
        self.call("ensure_product", json!({ "product_key": product_key }))
    }

    /// Link a project into a product for product-scoped search/inbox.
    pub fn products_link(&mut self, product_key: &str, project_key: &str) -> Result<Value> {
        self.call(
            "products_link",
            json!({
                "product_key": product_key,
                "project_key": project_key,
            }),
        )
    }

    /// Search messages across all projects linked to a product.
    pub fn search_product(&mut self, product_key: &str, query: &str, limit: u32) -> Result<Value> {
        self.call(
            "search_messages_product",
            json!({
                "product_key": product_key,
                "query": query,
                "limit": limit,
            }),
        )
    }

    /// Fetch inbox across projects linked to a product.
    pub fn inbox_product(
        &mut self,
        product_key: &str,
        agent_name: &str,
        limit: u32,
    ) -> Result<Value> {
        self.call(
            "fetch_inbox_product",
            json!({
                "product_key": product_key,
                "agent_name": agent_name,
                "limit": limit,
            }),
        )
    }

    /// Summarize a thread in product scope.
    #[allow(dead_code)] // CLI product summarize deferred; wrapper ready for patterns
    pub fn summarize_thread_product(
        &mut self,
        product_key: &str,
        thread_id: &str,
        llm_mode: bool,
    ) -> Result<Value> {
        self.call(
            "summarize_thread_product",
            json!({
                "product_key": product_key,
                "thread_id": thread_id,
                "llm_mode": llm_mode,
            }),
        )
    }

    /// Acquire an advisory build slot (agent-mail `acquire_build_slot` tool).
    /// Returns `{granted: [...]}` or `{conflicts: [...]}` — advisory, not
    /// hard-blocking. Used to serialize `cargo build`/`cargo test` across
    /// parallel workers on the shared `target/` dir (bd-mail-build-slot-gating).
    pub fn acquire_slot(
        &mut self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
        exclusive: bool,
        ttl_seconds: u64,
    ) -> Result<Value> {
        self.call(
            "acquire_build_slot",
            json!({
                "project_key": project_key,
                "agent_name": agent_name,
                "slot": slot,
                "exclusive": exclusive,
                "ttl_seconds": ttl_seconds,
            }),
        )
    }

    /// Extend a build-slot lease (`renew_build_slot`). Idempotent; safe to
    /// over-call. A slot auto-expires at its TTL if not renewed, so a forgotten
    /// release can't deadlock peers forever.
    ///
    /// Not yet wired to the loop or trait (the MailBackend trait has only
    /// slot_acquire/slot_release); this wrapper exists for agents/CLI use and
    /// the loop's future renewal cadence. #[allow(dead_code)] until a caller
    /// needs it.
    #[allow(dead_code)]
    pub fn renew_slot(
        &mut self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
        extend_seconds: u64,
    ) -> Result<Value> {
        self.call(
            "renew_build_slot",
            json!({
                "project_key": project_key,
                "agent_name": agent_name,
                "slot": slot,
                "extend_seconds": extend_seconds,
            }),
        )
    }

    /// Release a build slot (`release_build_slot`). Non-destructive: the
    /// release record is kept for audit. Critical to call after a build so the
    /// TTL doesn't block peers (the TTL is the safety net, release is the norm).
    pub fn release_slot(
        &mut self,
        project_key: &str,
        agent_name: &str,
        slot: &str,
    ) -> Result<Value> {
        self.call(
            "release_build_slot",
            json!({
                "project_key": project_key,
                "agent_name": agent_name,
                "slot": slot,
            }),
        )
    }

    /// Extend the TTL of active reservations (agent-mail MCP
    /// `renew_file_reservations`). Renewing an already-fresh lease is a no-op
    /// — safe to over-call every heartbeat. `paths` / `ids` scope the renewal
    /// to specific reservations; `None`/`None` renews ALL of this agent's
    /// active reservations in the project. The returned JSON carries the
    /// renewed list (`{"renewed": N, "file_reservations": [...]}`) — see
    /// [`list_own_reservations`].
    ///
    /// ADR-0001 gap A3 (bd-mail-lease-renewal-durable): long loops must not
    /// lose their lease mid-run.
    pub fn renew(
        &mut self,
        project_key: &str,
        agent_name: &str,
        extend_seconds: u64,
        paths: Option<&[String]>,
        ids: Option<&[u64]>,
    ) -> Result<Value> {
        self.call(
            "renew_file_reservations",
            renew_args(project_key, agent_name, extend_seconds, paths, ids),
        )
    }

    // ---- pane identity bridge (bd-mail-pane-identity-bridge, ADR-0001 A4) ----

    /// Write the per-pane identity file the agent-mail server reads for
    /// `resolve_pane_identity`: `<config>/agent-mail/identity/<project_hash>/
    /// <pane_id>` containing `<name>\n`. A filesystem op, not an MCP tool —
    /// flywheel's own bridge, because `register_agent` only writes identities
    /// for *tmux* panes, and herdr panes are not tmux panes. The format is
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

    /// Resolve the agent name for a pane via the agent-mail MCP
    /// `resolve_pane_identity` tool — the server reads the identity file
    /// written by [`Mail::write_pane_identity`]. Returns the tool's inner
    /// JSON (`{agent_name, pane_id, identity_path}`); when no identity exists
    /// the tool returns an isError text, which comes back as a non-JSON
    /// string value (no `agent_name` key) — callers treat that as not-found.
    pub fn resolve_pane_identity(&mut self, project_key: &str, pane_id: &str) -> Result<Value> {
        self.call(
            "resolve_pane_identity",
            json!({ "project_key": project_key, "pane_id": pane_id }),
        )
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
// exact `identity_path` returned). The whole mechanism is disposable at R1:
// toron ADR-0003 §G9 resolves pane identity backend-natively.

#[cfg(test)]
static TEST_IDENTITY_BASE: std::sync::LazyLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn test_identity_base() -> Option<PathBuf> {
    TEST_IDENTITY_BASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
fn set_test_identity_base(path: Option<PathBuf>) {
    *TEST_IDENTITY_BASE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
}

/// Config base for identity files: `$XDG_CONFIG_HOME` when set, else
/// `~/.config` — identical to agent-mail core's `config_base_dir()`, so the
/// bridge writes exactly where the server reads.
fn identity_base() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = test_identity_base() {
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
/// Verified against the live server: `/Users/bangedorrunt/workspace/flywheel`
/// → `81e9e2338899`.
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

/// Write identity content with 0600 permissions (the file carries an agent
/// identity — owner-only by default).
#[cfg(unix)]
fn write_identity_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content)
}

#[cfg(not(unix))]
fn write_identity_file(path: &Path, content: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, content)
}

/// Build the `renew_file_reservations` argument envelope. Optional groups are
/// omitted (not null) when absent — agent-mail rejects unknown/null members.
fn renew_args(
    project_key: &str,
    agent_name: &str,
    extend_seconds: u64,
    paths: Option<&[String]>,
    ids: Option<&[u64]>,
) -> Value {
    let mut args = json!({
        "project_key": project_key,
        "agent_name": agent_name,
        "extend_seconds": extend_seconds,
    });
    if let Some(p) = paths {
        args["paths"] = json!(p);
    }
    if let Some(i) = ids {
        args["file_reservation_ids"] = json!(i);
    }
    args
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_parses_text_json() {
        let resp = json!({
            "jsonrpc": "2.0", "id": "1",
            "result": { "content": [ { "type": "text", "text": "{\"ok\": true, \"n\": 3}" } ] }
        });
        let v = tool_result(resp).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["n"], 3);
    }

    #[test]
    fn tool_result_falls_back_to_raw_text() {
        let resp = json!({
            "jsonrpc": "2.0", "id": "1",
            "result": { "content": [ { "type": "text", "text": "hello world" } ] }
        });
        let v = tool_result(resp).unwrap();
        assert_eq!(v, json!("hello world"));
    }

    #[test]
    fn error_response_surfaces() {
        let resp = json!({
            "jsonrpc": "2.0", "id": "1",
            "error": { "code": -32000, "message": "boom" }
        });
        let err = tool_result(resp).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    /// `Mail::register_agent` exists and is callable (compile-time guard
    /// against accidental removal; the envelope mirrors the existing
    /// `ensure_project`/`send` wrappers exactly — live round-trip is
    /// backend-parity-test integration territory). Constructed with a dummy
    /// URL; no HTTP is made until the method is actually invoked.
    #[test]
    fn register_agent_method_exists() {
        use crate::config::MailConfig;
        let mut mail = Mail::from_config(Some(&MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test".into()),
        }))
        .expect("from_config ok")
        .expect("Some(Mail)");
        // take a reference to the fn to prove it exists with the right signature;
        // do NOT call it (would hit the dummy URL and fail).
        let _: fn(&mut Mail, &str, &str, &str, &str, &str) -> Result<Value> = Mail::register_agent;
        let _ = &mut mail; // silence unused-mut
    }

    /// The three slot wrappers exist with the right signatures (compile-time
    /// guard against accidental removal; envelopes mirror the existing
    /// wrappers — live round-trip is backend-parity-test territory).
    #[test]
    fn slot_wrappers_exist() {
        let _: fn(&mut Mail, &str, &str, &str, bool, u64) -> Result<Value> = Mail::acquire_slot;
        let _: fn(&mut Mail, &str, &str, &str, u64) -> Result<Value> = Mail::renew_slot;
        let _: fn(&mut Mail, &str, &str, &str) -> Result<Value> = Mail::release_slot;
    }

    /// Mail::search exists with the right signature (bd-mail-search-summarize).
    /// review-fix F1: gained the `include_body_md` bool param.
    #[test]
    fn search_wrapper_exists() {
        let _: fn(
            &mut Mail,
            &str,
            &str,
            u32,
            Option<&str>,
            Option<&str>,
            Option<&str>,
            Option<&str>,
            Option<&str>,
            Option<&str>,
            bool,
        ) -> Result<Value> = Mail::search;
    }

    /// review-fix F1: search_messages must request the body when asked.
    /// Builds the args JSON via the same path as the live call and asserts
    /// the `include_body_md` flag is emitted exactly when requested.
    #[test]
    fn search_sends_include_body_md() {
        // with the flag: args must carry include_body_md: true
        let args = Mail::search_args(
            "proj",
            "q",
            1,
            Some("sender"),
            None,
            Some("thread-1"),
            None,
            None,
            Some("recency"),
            true,
        );
        assert_eq!(args["include_body_md"], json!(true));
        assert_eq!(args["thread_id"], json!("thread-1"));
        assert_eq!(args["sender"], json!("sender"));
        // without the flag: no include_body_md key
        let args = Mail::search_args("proj", "q", 1, None, None, None, None, None, None, false);
        assert!(args.get("include_body_md").is_none());
    }

    /// Cross-project contact + product wrappers (bd-mail-cross-project-contact).
    #[test]
    fn cross_project_wrappers_exist() {
        let _: fn(&mut Mail, &str, &str, &str, Option<&str>, &str, Option<u64>) -> Result<Value> =
            Mail::request_contact;
        let _: fn(&mut Mail, &str, &str, &str, bool) -> Result<Value> = Mail::respond_contact;
        let _: fn(&mut Mail, &str, &str) -> Result<Value> = Mail::list_contacts;
        let _: fn(&mut Mail, &str, &str, &str) -> Result<Value> = Mail::set_contact_policy;
        let _: fn(&mut Mail, &str) -> Result<Value> = Mail::ensure_product;
        let _: fn(&mut Mail, &str, &str) -> Result<Value> = Mail::products_link;
        let _: fn(&mut Mail, &str, &str, u32) -> Result<Value> = Mail::search_product;
        let _: fn(&mut Mail, &str, &str, u32) -> Result<Value> = Mail::inbox_product;
        let _: fn(&mut Mail, &str, &str, bool) -> Result<Value> = Mail::summarize_thread_product;
    }

    // ---- renew (bd-mail-lease-renewal-durable) ----

    /// `renew` envelope: required params always present; optional `paths` /
    /// `ids` included only when `Some`. This is the exact JSON the MCP tool
    /// call will carry (verified against agent-mail's
    /// `renew_file_reservations` schema: project_key + agent_name required,
    /// extend_seconds int, paths / file_reservation_ids optional).
    #[test]
    fn renew_args_envelope_shape() {
        // paths + ids present
        let v = renew_args(
            "/proj",
            "Orchid",
            1800,
            Some(&["src/a.rs".into(), "src/b.rs".into()]),
            Some(&[7, 9]),
        );
        assert_eq!(v["project_key"], "/proj");
        assert_eq!(v["agent_name"], "Orchid");
        assert_eq!(v["extend_seconds"], 1800);
        assert_eq!(v["paths"], json!(["src/a.rs", "src/b.rs"]));
        assert_eq!(v["file_reservation_ids"], json!([7, 9]));

        // both optional groups omitted
        let v = renew_args("/proj", "Orchid", 60, None, None);
        assert_eq!(v["project_key"], "/proj");
        assert_eq!(v["agent_name"], "Orchid");
        assert_eq!(v["extend_seconds"], 60);
        assert!(v.get("paths").is_none(), "paths must be omitted when None");
        assert!(
            v.get("file_reservation_ids").is_none(),
            "ids must be omitted when None"
        );

        // only ids
        let v = renew_args("/proj", "Orchid", 60, None, Some(&[3]));
        assert!(v.get("paths").is_none());
        assert_eq!(v["file_reservation_ids"], json!([3]));
    }

    /// Signature of [`Mail::renew`] (type alias keeps the compile-time guard
    /// readable; clippy rejects the inline fn type as too complex).
    type RenewFn =
        fn(&mut Mail, &str, &str, u64, Option<&[String]>, Option<&[u64]>) -> Result<Value>;

    /// `renew` is callable with the full signature (compile-time guard; no
    /// HTTP — dummy URL).
    #[test]
    fn renew_method_exists() {
        use crate::config::MailConfig;
        let mut mail = Mail::from_config(Some(&MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test".into()),
        }))
        .expect("from_config ok")
        .expect("Some(Mail)");
        let _: RenewFn = Mail::renew;
        let _ = &mut mail; // silence unused-mut
    }

    /// `list_own_reservations` parses the renew response into the renewed
    /// reservation list (agent-mail exposes NO list-reservations MCP tool —
    /// verified against the full tools/list surface — so the renew response
    /// itself is the authoritative "what did I renew" source). Captured live
    /// response shape from agent-mail 2026-08-05.
    #[test]
    fn list_own_reservations_parses_renew_response() {
        let resp = json!({
            "renewed": 2,
            "file_reservations": [
                {"id": 195, "path_pattern": "src/probe.tmp", "old_expires_ts": "2026-08-05T04:07:33Z", "new_expires_ts": "2026-08-05T04:08:33Z"},
                {"id": 196, "path_pattern": "loops.json", "old_expires_ts": "2026-08-05T04:07:33Z", "new_expires_ts": "2026-08-05T04:08:33Z"}
            ]
        });
        let list = list_own_reservations(&resp);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, 195);
        assert_eq!(list[0].path_pattern, "src/probe.tmp");
        assert_eq!(list[0].new_expires_ts, "2026-08-05T04:08:33Z");
        assert_eq!(list[1].id, 196);
        assert_eq!(list[1].path_pattern, "loops.json");
    }

    /// Empty renew response → empty list (heartbeat logs "renewed 0").
    #[test]
    fn list_own_reservations_empty_response() {
        assert!(list_own_reservations(&json!({"renewed": 0})).is_empty());
        assert!(list_own_reservations(&json!({})).is_empty());
    }

    /// Live lease-renewal round-trip (bd-mail-lease-renewal-durable
    /// acceptance criterion): a TTL-60s reservation is renewed at t=45s and
    /// still active at t=90s, while the no-renew control expires at t=60s.
    /// Proves the 4h-loop/1h-lease scenario at 1/40 scale. Runs against the
    /// real agent-mail server from the flywheel config:
    /// `cargo test -- --ignored renew_lease_survives_renewal`
    ///
    /// Discriminator: the conflicts tool excludes SELF-owned reservations
    /// (an owner sees its own path as conflict-free), so a second identity
    /// ("observer") performs the conflict check — an alive lease conflicts
    /// with the observer, an expired one does not.
    #[test]
    #[ignore = "needs a live agent-mail server (reads flywheel config)"]
    fn renew_lease_survives_renewal() {
        let cfg = crate::config::Config::load().expect("config loads");
        let mut mail = Mail::from_config(cfg.agent_mail.as_ref())
            .expect("from_config ok")
            .expect("agent_mail configured");
        let mut observer = Mail {
            name: "SapphireOwl".into(),
            base: mail.base.clone(),
            token: mail.token.clone(),
            next_id: 1,
        };
        let key = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .to_str()
            .unwrap()
            .to_string();
        mail.ensure_project(&key).expect("ensure_project");
        observer.ensure_project(&key).expect("ensure_project");
        // the configured identity must exist in the project for lease ops
        // (register_agent is idempotent on name)
        let cfg_name = mail.name.clone();
        let obs_name = observer.name.clone();
        mail.register_agent(&key, &cfg_name, "flywheel", "flywheel", "renew-live-probe")
            .expect("register configured identity");
        observer
            .register_agent(
                &key,
                &obs_name,
                "flywheel",
                "flywheel",
                "renew-live-probe-observer",
            )
            .expect("register observer identity");

        let path = format!("target/renew-live-probe-{}.tmp", std::process::id());
        let _ = mail.release(&key); // clear any stale probe from a prior run

        // --- renewed arm: TTL 60, renew +60 at t=45 → still active at t=90
        mail.reserve(
            &key,
            std::slice::from_ref(&path),
            60,
            true,
            "renew-live-probe",
        )
        .expect("reserve");
        std::thread::sleep(Duration::from_secs(45));
        let my_name = mail.name.clone();
        // scope by path — an unscoped renew touches ALL of this agent's
        // active reservations (by design), which would include the live
        // test's own file reservations
        let r = mail
            .renew(&key, &my_name, 60, Some(std::slice::from_ref(&path)), None)
            .expect("renew");
        assert_eq!(r["renewed"].as_u64(), Some(1), "exactly one renewed: {r}");
        let renewed = list_own_reservations(&r);
        assert_eq!(renewed.len(), 1);
        assert_eq!(renewed[0].path_pattern, path);
        std::thread::sleep(Duration::from_secs(45)); // t=90 > original expiry t=60
        let conflicts = observer
            .conflicts(&key, std::slice::from_ref(&path))
            .expect("conflicts check");
        assert_eq!(
            conflicts["conflict_free"],
            json!(false),
            "renewed lease must still be active at t=90: {conflicts}"
        );
        mail.release(&key).expect("release");

        // --- control arm: TTL 60, no renew → gone by t=60 (checked at t=62)
        mail.reserve(
            &key,
            std::slice::from_ref(&path),
            60,
            true,
            "renew-live-probe-control",
        )
        .expect("reserve");
        std::thread::sleep(Duration::from_secs(62));
        let conflicts = observer
            .conflicts(&key, std::slice::from_ref(&path))
            .expect("conflicts check");
        assert_eq!(
            conflicts["conflict_free"],
            json!(true),
            "control lease must have expired by t=62: {conflicts}"
        );
        mail.release(&key).expect("release");
    }

    // ---- pane identity bridge (bd-mail-pane-identity-bridge) ----

    fn test_mail() -> Mail {
        Mail::from_config(Some(&crate::config::MailConfig {
            url: Some("http://127.0.0.1:9999".into()),
            token: None,
            name: Some("test".into()),
        }))
        .expect("from_config ok")
        .expect("Some(Mail)")
    }

    /// `project_hash` matches agent-mail's derivation exactly: truncated
    /// lower-hex SHA-1 of the canonicalized absolute project path. The
    /// `/a/b/c` fixture is machine-independent (no canonicalization — the
    /// path doesn't exist); the live server-verified value for this repo's
    /// manifest dir is pinned by `pane_identity_roundtrip_via_mcp`.
    #[test]
    fn project_hash_matches_agent_mail_fixture() {
        assert_eq!(project_hash("/a/b/c"), "6dc15334741d");
    }

    /// Existing absolute paths are canonicalized BEFORE hashing (agent-mail
    /// parity — a symlinked checkout must hash identically to its target).
    #[test]
    fn project_hash_canonicalizes_before_hashing() {
        let dir = std::env::temp_dir().join(format!("fw-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        let canon = std::fs::canonicalize(&dir).expect("canonicalize");
        let mut hasher = Sha1::new();
        hasher.update(canon.to_string_lossy().as_bytes());
        let expected: String = hex::encode(hasher.finalize()).chars().take(12).collect();
        assert_eq!(
            project_hash(dir.to_str().expect("utf8 path")),
            expected,
            "hash must be over the canonicalized path"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn project_hash_is_twelve_lower_hex_chars() {
        let h = project_hash("/a/b/c");
        assert_eq!(h.len(), 12);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "must be lower hex: {h}"
        );
    }

    #[test]
    fn sanitize_pane_id_matches_agent_mail() {
        // herdr ids: workspace:pane
        assert_eq!(sanitize_pane_id("w1:p1"), "w1-p1");
        assert_eq!(sanitize_pane_id("w13:p1"), "w13-p1");
        // tmux ids: %-prefixed, composite keys
        assert_eq!(sanitize_pane_id("%0"), "0");
        assert_eq!(sanitize_pane_id("main:0:2"), "main-0-2");
        // other unsafe chars
        assert_eq!(sanitize_pane_id("a b/c"), "a_b_c");
        assert_eq!(sanitize_pane_id(""), "unknown");
    }

    /// FS round-trip under a controlled test base (the cfg(test) override
    /// keeps real `~/.config` untouched). Runs the whole lifecycle in one
    /// test because the base override is process-global — parallel tests
    /// would race it.
    #[test]
    fn pane_identity_fs_lifecycle() {
        let base = std::env::temp_dir().join(format!("fw-identity-{}", std::process::id()));
        set_test_identity_base(Some(base.clone()));
        let mail = test_mail();

        // canonical path shape
        let path = canonical_identity_path("/a/b/c", "w1:p1");
        assert_eq!(
            path,
            base.join("agent-mail")
                .join("identity")
                .join("6dc15334741d")
                .join("w1-p1")
        );

        // write: name trimmed, single line + newline, 0600 perms
        let written = mail
            .write_pane_identity("/a/b/c", "w1:p1", "  TestAgent  ")
            .expect("write ok");
        assert_eq!(written, path);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read identity"),
            "TestAgent\n",
            "content must be the trimmed name + newline (server format)"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
                0o600,
                "identity file must be owner-only"
            );
        }

        // empty name is refused (would produce an unresolvable empty file)
        let err = mail
            .write_pane_identity("/a/b/c", "w1:p1", "   ")
            .expect_err("empty name must fail");
        assert!(err.to_string().contains("empty"));

        // symlinked leaf is refused (agent-mail core parity — no clobbering
        // a symlink someone else planted)
        let other = canonical_identity_path("/a/b/c", "w1:p2");
        std::fs::create_dir_all(other.parent().expect("parent")).expect("mkdir");
        let target =
            std::env::temp_dir().join(format!("fw-identity-target-{}", std::process::id()));
        std::fs::write(&target, "OriginalAgent\n").expect("write target");
        std::os::unix::fs::symlink(&target, &other).expect("symlink");
        let err = mail
            .write_pane_identity("/a/b/c", "w1:p2", "NewAgent")
            .expect_err("symlink leaf must be refused");
        assert!(err.to_string().contains("symlink"));
        assert_eq!(
            std::fs::read_to_string(&target).expect("read target"),
            "OriginalAgent\n",
            "symlink target must be untouched"
        );

        // clear removes the file; a second clear (duplicate pane_exited) is a
        // no-op, not an error
        let cleared = mail
            .clear_pane_identity("/a/b/c", "w1:p1")
            .expect("clear ok");
        assert_eq!(cleared, path);
        assert!(!path.exists(), "identity file must be gone after clear");
        let again = mail
            .clear_pane_identity("/a/b/c", "w1:p1")
            .expect("clear twice ok");
        assert_eq!(again, path);

        set_test_identity_base(None);
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_file(&target).ok();
    }

    /// `Mail::resolve_pane_identity` exists and is callable (compile-time
    /// guard against accidental removal; no HTTP — dummy URL).
    #[test]
    fn resolve_pane_identity_method_exists() {
        let mut mail = test_mail();
        let _: fn(&mut Mail, &str, &str) -> Result<Value> = Mail::resolve_pane_identity;
        let _ = &mut mail; // silence unused-mut
    }

    /// Live round-trip (bd-mail-pane-identity-bridge acceptance criterion):
    /// write the identity file → the agent-mail server resolves it via the
    /// MCP tool (returning OUR file's path) → clear → not found. Also pins
    /// the manifest-dir hash against the real server — if flywheel ever
    /// hashes differently, the server's resolved `identity_path` won't match
    /// the path we wrote and this test fails. Runs against the real server
    /// from the flywheel config:
    /// `cargo test -- --ignored pane_identity_roundtrip_via_mcp`
    #[test]
    #[ignore = "needs a live agent-mail server (reads flywheel config)"]
    fn pane_identity_roundtrip_via_mcp() {
        let cfg = crate::config::Config::load().expect("config loads");
        let mut mail = Mail::from_config(cfg.agent_mail.as_ref())
            .expect("from_config ok")
            .expect("agent_mail configured");
        let key = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .to_str()
            .expect("manifest dir is utf8")
            .to_string();
        mail.ensure_project(&key).expect("ensure_project");
        // the configured identity must exist in the project (register_agent
        // is idempotent on name)
        let cfg_name = mail.name.clone();
        mail.register_agent(
            &key,
            &cfg_name,
            "flywheel",
            "flywheel",
            "pane-identity-live-probe",
        )
        .expect("register configured identity");

        let pane = format!("w9:p{}", std::process::id()); // unique per run
        let written = mail
            .write_pane_identity(&key, &pane, &cfg_name)
            .expect("write_pane_identity");
        let resolved = mail
            .resolve_pane_identity(&key, &pane)
            .expect("resolve_pane_identity call");
        assert_eq!(
            resolved["agent_name"],
            json!(cfg_name),
            "server must resolve the bridged name: {resolved}"
        );
        assert_eq!(
            resolved["identity_path"],
            json!(written.to_string_lossy()),
            "server must read OUR file — hash/format contract broken: {resolved}"
        );

        mail.clear_pane_identity(&key, &pane)
            .expect("clear_pane_identity");
        let after = mail
            .resolve_pane_identity(&key, &pane)
            .expect("resolve after clear");
        assert!(
            after.get("agent_name").is_none(),
            "stale lookup after clear — identity file must be gone: {after}"
        );
    }

    // ---- reply + thread continuity (bd-mail-reply-thread) ----

    /// Debate-loop acceptance: A opens a thread (thread_id set), B replies
    /// (default to the original sender), A counters, then the judge reads
    /// the whole thread via summarize_thread — all messages under ONE
    /// thread_id, both participants visible, count matches 3. Runs against
    /// the live agent-mail server:
    ///   `cargo test -- --ignored debate_loop_thread_continuity`
    #[test]
    #[ignore = "needs a live agent-mail server (reads flywheel config)"]
    fn debate_loop_thread_continuity() {
        let cfg = crate::config::Config::load().expect("config loads");
        let mut mail = Mail::from_config(cfg.agent_mail.as_ref())
            .expect("from_config ok")
            .expect("agent_mail configured");
        let mut observer = Mail {
            name: "SapphireOwl".into(),
            base: mail.base.clone(),
            token: mail.token.clone(),
            next_id: 1,
        };
        let key = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .to_str()
            .unwrap()
            .to_string();
        mail.ensure_project(&key).expect("ensure_project");
        observer.ensure_project(&key).expect("ensure_project");
        let cfg_name = mail.name.clone();
        let obs_name = observer.name.clone();
        mail.register_agent(&key, &cfg_name, "flywheel", "flywheel", "debate-live-probe")
            .expect("register A");
        observer
            .register_agent(
                &key,
                &obs_name,
                "flywheel",
                "flywheel",
                "debate-live-probe-observer",
            )
            .expect("register B");

        let thread = format!("debate-live-{}", std::process::id()); // unique per run
                                                                    // round 1: A opens the thread with thread_id set
        let a1 = mail
            .send(
                &key,
                std::slice::from_ref(&obs_name),
                "debate round 1",
                "A: for",
                "normal",
                false,
                Some(&thread),
            )
            .expect("A send");
        let a1_id = a1["deliveries"][0]["payload"]["id"]
            .as_u64()
            .expect("A message id: {a1}");
        // round 2: B replies to A's message, defaulting to the original sender
        let b1 = observer
            .reply(&key, a1_id, "B: against", None)
            .expect("B reply");
        let b1_id = b1["deliveries"][0]["payload"]["id"]
            .as_u64()
            .expect("B message id: {b1}");
        // round 3: A counters B's reply (thread continuity via reply_message)
        let a2 = mail
            .reply(&key, b1_id, "A: rebuttal", None)
            .expect("A counter");
        let a2_id = a2["deliveries"][0]["payload"]["id"]
            .as_u64()
            .expect("A message id: {a2}");

        // judge reads the thread compactly
        let sum = mail
            .summarize_thread(&key, &thread, false)
            .expect("summarize_thread");
        assert_eq!(sum["thread_id"], json!(thread), "same thread: {sum}");
        let participants = sum["summary"]["participants"]
            .as_array()
            .expect("participants list");
        let names: Vec<&str> = participants.iter().filter_map(|p| p.as_str()).collect();
        assert!(
            names.contains(&cfg_name.as_str()) && names.contains(&obs_name.as_str()),
            "both participants in thread: {names:?}"
        );
        assert!(
            sum["summary"]["total_messages"].as_u64() >= Some(3),
            "3+ rounds under one thread: {sum}"
        );
        let _ = (a1_id, b1_id, a2_id); // ids are the round evidence
    }

    // ---- ack + mark_read + SLA (bd-mail-ack-sla-tracking) ----

    /// Ack/SLA acceptance: ack_required message round-trips through
    /// mark_read (read_ts set) and ack (ack_ts set), and a fresh
    /// ack_required message is NOT ack_overdue (no false positive). The
    /// overdue side of the SLA is proven by the live probe: an
    /// ack_required message older than the 30-min window appears in
    /// `ack_overdue_only` results (verified manually against the server).
    ///   `cargo test -- --ignored ack_mark_read_roundtrip`
    #[test]
    #[ignore = "needs a live agent-mail server (reads flywheel config)"]
    fn ack_mark_read_roundtrip() {
        let cfg = crate::config::Config::load().expect("config loads");
        let mut mail = Mail::from_config(cfg.agent_mail.as_ref())
            .expect("from_config ok")
            .expect("agent_mail configured");
        let mut observer = Mail {
            name: "SapphireOwl".into(),
            base: mail.base.clone(),
            token: mail.token.clone(),
            next_id: 1,
        };
        let key = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .to_str()
            .unwrap()
            .to_string();
        mail.ensure_project(&key).expect("ensure_project");
        observer.ensure_project(&key).expect("ensure_project");
        let cfg_name = mail.name.clone();
        let obs_name = observer.name.clone();
        mail.register_agent(&key, &cfg_name, "flywheel", "flywheel", "ack-live-probe")
            .expect("register A");
        observer
            .register_agent(
                &key,
                &obs_name,
                "flywheel",
                "flywheel",
                "ack-live-probe-observer",
            )
            .expect("register B");

        // A sends an ack_required ping to B
        let sent = mail
            .send(
                &key,
                std::slice::from_ref(&obs_name),
                "ack roundtrip",
                "ping — ack required",
                "high",
                true,
                None,
            )
            .expect("send");
        let msg_id = sent["deliveries"][0]["payload"]["id"]
            .as_u64()
            .expect("message id");

        // fresh message must NOT be overdue yet
        let overdue = observer.ack_overdue(&key, &obs_name).expect("ack_overdue");
        let overdue_ids: Vec<u64> = overdue
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_u64))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !overdue_ids.contains(&msg_id),
            "fresh ack_required message must not be overdue: {overdue}"
        );

        // B marks read (seen, not acted)
        observer
            .mark_read(&key, &obs_name, msg_id)
            .expect("mark_read");
        let inbox = observer
            .inbox_filtered(
                &key,
                &obs_name,
                &MailInboxFilter {
                    urgent_only: false,
                    unread_only: false,
                    ack_overdue_only: false,
                    include_bodies: true,
                    limit: 20,
                    since_ts: None,
                },
            )
            .expect("inbox after mark_read");
        let m = inbox
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|m| m.get("id").and_then(Value::as_u64) == Some(msg_id))
            })
            .expect("message in inbox");
        assert!(
            m.get("read_ts").and_then(Value::as_str).is_some(),
            "mark_read must set read_ts: {m}"
        );
        assert!(
            m.get("ack_ts").and_then(Value::as_str).is_none(),
            "mark_read must NOT set ack_ts (seen != acted): {m}"
        );

        // B acks (acted)
        observer.ack(&key, &obs_name, msg_id).expect("ack");
        let inbox = observer
            .inbox_filtered(
                &key,
                &obs_name,
                &MailInboxFilter {
                    urgent_only: false,
                    unread_only: false,
                    ack_overdue_only: false,
                    include_bodies: true,
                    limit: 20,
                    since_ts: None,
                },
            )
            .expect("inbox after ack");
        let m = inbox
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|m| m.get("id").and_then(Value::as_u64) == Some(msg_id))
            })
            .expect("message in inbox");
        assert!(
            m.get("ack_ts").and_then(Value::as_str).is_some(),
            "ack must set ack_ts: {m}"
        );
    }
}

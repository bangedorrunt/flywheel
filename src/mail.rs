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

    pub fn send(
        &mut self,
        project_key: &str,
        to: &[String],
        subject: &str,
        body: &str,
        importance: &str,
        ack: bool,
    ) -> Result<Value> {
        self.call(
            "send_message",
            json!({
                "project_key": project_key, "sender_name": self.name, "to": to,
                "subject": subject, "body_md": body, "importance": importance,
                "ack_required": ack
            }),
        )
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
        self.call("renew_file_reservations", renew_args(
            project_key,
            agent_name,
            extend_seconds,
            paths,
            ids,
        ))
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
    type RenewFn = fn(
        &mut Mail,
        &str,
        &str,
        u64,
        Option<&[String]>,
        Option<&[u64]>,
    ) -> Result<Value>;

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
            .register_agent(&key, &obs_name, "flywheel", "flywheel", "renew-live-probe-observer")
            .expect("register observer identity");

        let path = format!("target/renew-live-probe-{}.tmp", std::process::id());
        let _ = mail.release(&key); // clear any stale probe from a prior run

        // --- renewed arm: TTL 60, renew +60 at t=45 → still active at t=90
        mail.reserve(&key, std::slice::from_ref(&path), 60, true, "renew-live-probe")
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
            conflicts["conflict_free"], json!(false),
            "renewed lease must still be active at t=90: {conflicts}"
        );
        mail.release(&key).expect("release");

        // --- control arm: TTL 60, no renew → gone by t=60 (checked at t=62)
        mail.reserve(&key, std::slice::from_ref(&path), 60, true, "renew-live-probe-control")
            .expect("reserve");
        std::thread::sleep(Duration::from_secs(62));
        let conflicts = observer
            .conflicts(&key, std::slice::from_ref(&path))
            .expect("conflicts check");
        assert_eq!(
            conflicts["conflict_free"], json!(true),
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
            h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
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
        let target = std::env::temp_dir().join(format!("fw-identity-target-{}", std::process::id()));
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
        mail.register_agent(&key, &cfg_name, "flywheel", "flywheel", "pane-identity-live-probe")
            .expect("register configured identity");

        let pane = format!("w9:p{}", std::process::id()); // unique per run
        let written = mail
            .write_pane_identity(&key, &pane, &cfg_name)
            .expect("write_pane_identity");
        let resolved = mail
            .resolve_pane_identity(&key, &pane)
            .expect("resolve_pane_identity call");
        assert_eq!(
            resolved["agent_name"], json!(cfg_name),
            "server must resolve the bridged name: {resolved}"
        );
        assert_eq!(
            resolved["identity_path"],
            json!(written.to_string_lossy()),
            "server must read OUR file — hash/format contract broken: {resolved}"
        );

        mail.clear_pane_identity(&key, &pane).expect("clear_pane_identity");
        let after = mail
            .resolve_pane_identity(&key, &pane)
            .expect("resolve after clear");
        assert!(
            after.get("agent_name").is_none(),
            "stale lookup after clear — identity file must be gone: {after}"
        );
    }
}

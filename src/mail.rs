//! agent-mail coordination client — JSON-RPC over HTTP against the MCP endpoint.
//! Server: http://127.0.0.1:8765/mcp/ (bearer token from config). Tools return
//! JSON-serialized text content; we deserialize the inner JSON.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
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
}

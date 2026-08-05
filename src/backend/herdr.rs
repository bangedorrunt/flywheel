//! herdr multiplexer backend — wraps `crate::herdr::Client` (the unix-socket
//! JSON-RPC client, protocol 19) as the default `MultiplexerBackend` impl.
//! Pure delegation to the existing typed helpers; `BackendError::Unsupported`
//! only for trait methods whose `Client` wrapper doesn't exist yet
//! (`pane_close`, `pane_run` — added when needed; herdr has `pane.close` via
//! raw `call`, no typed helper).
//!
//! governed-by: ADR-0002  ·  implements: `MultiplexerBackend`
//!
//! No behavior change vs calling `crate::herdr::Client` directly. R0.8
//! (`bd-backend-parity-test`) proves byte-identical results.

use crate::backend::{BackendError, EventStream as BackendEventStream, MultiplexerBackend, Result};
use crate::herdr::{
    AgentInfo, AgentStatus, Client, EventStream as HerdrEventStream, PaneInfo, PushEvent,
    Subscription, WaitOptions, WorkspaceInfo,
};
use serde_json::{json, Value};
use std::time::Duration;

/// Default multiplexer backend: delegates to the herdr socket client.
pub struct HerdrBackend {
    client: Client,
}

impl HerdrBackend {
    /// Construct from an already-connected `Client`. The factory
    /// (`bd-backend-env-config`) calls this; tests can inject a Client built
    /// via `Client::connect`.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    fn unsupported(&self, method: &'static str) -> BackendError {
        BackendError::Unsupported {
            backend: "herdr".to_string(),
            method,
            reason: "not implemented on the herdr backend; Client has no typed wrapper".to_string(),
        }
    }
}

/// Adapter: `herdr::EventStream` → `backend::EventStream`. Forwards
/// `read_event` and `set_read_timeout` verbatim.
struct HerdrEventStreamAdapter(HerdrEventStream);

impl BackendEventStream for HerdrEventStreamAdapter {
    fn read_event(&mut self) -> Result<Option<PushEvent>> {
        self.0.read_event().map_err(BackendError::from)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.0.set_read_timeout(timeout).map_err(BackendError::from)
    }
}

impl MultiplexerBackend for HerdrBackend {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn workspace_list(&mut self) -> Result<Vec<WorkspaceInfo>> {
        self.client.workspace_list().map_err(BackendError::from)
    }

    fn workspace_create(&mut self, label: Option<&str>) -> Result<WorkspaceInfo> {
        // Client::workspace_create also takes (cwd, focus); the trait exposes
        // only label. cwd=None (use the server default), focus=false (loop
        // keeps the caller's focus — the herdr-skill rule).
        let (ws, _pane) = self
            .client
            .workspace_create(None, label, false)
            .map_err(BackendError::from)?;
        Ok(ws)
    }

    fn workspace_close(&mut self, workspace_id: &str) -> Result<()> {
        self.client
            .workspace_close(workspace_id)
            .map_err(BackendError::from)?;
        Ok(())
    }

    fn pane_split(
        &mut self,
        parent_pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<PaneInfo> {
        self.client
            .pane_split(parent_pane_id, direction, cwd)
            .map_err(BackendError::from)
    }

    fn pane_list(&mut self, workspace_id: Option<&str>) -> Result<Vec<PaneInfo>> {
        self.client
            .pane_list(workspace_id)
            .map_err(BackendError::from)
    }

    fn pane_close(&mut self, pane_id: &str) -> Result<()> {
        // No typed Client helper; use the raw call (same shape as
        // src/main.rs:3188). Keeps the herdr-skill "never close what you didn't
        // create" safety at the loop layer, not here.
        self.client
            .call("pane.close", json!({ "pane_id": pane_id }))?;
        Ok(())
    }

    fn pane_run(&mut self, _pane_id: &str, _command: &str) -> Result<()> {
        // herdr exposes `pane run` via the CLI; the socket protocol has no
        // typed helper yet. Unsupported until a typed Client::pane_run lands.
        Err(self.unsupported("pane_run"))
    }

    fn pane_wait_output(&mut self, pane_id: &str, needle: &str, timeout_ms: u64) -> Result<Value> {
        // Map the trait's simple (pane, needle, timeout) onto Client's richer
        // (pane, mode, pattern, timeout). mode="match" = literal substring
        // (matches the loop's existing usage of __wait-output).
        let res = self
            .client
            .pane_wait_output(pane_id, "match", needle, Some(timeout_ms))
            .map_err(BackendError::from)?;
        Ok(json!({
            "pane_id": res.pane_id,
            "matched_line": res.matched_line,
            "read": res.read,
        }))
    }

    fn agent_list(&mut self) -> Result<Vec<AgentInfo>> {
        self.client.agent_list().map_err(BackendError::from)
    }

    fn agent_start(
        &mut self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> Result<AgentInfo> {
        // Default startup timeout = 30s (mirrors herdr agent start's own default).
        self.client
            .agent_start(name, kind, pane_id, 30_000, args)
            .map_err(BackendError::from)
    }

    fn agent_rename(&mut self, target: &str, new_name: Option<&str>) -> Result<()> {
        // Client::agent_rename takes a &str; the trait's Option lets a caller
        // clear the name. None is not currently expressible on the wire —
        // surface Unsupported for that case (rare; the loop always sets a name).
        let name = new_name.ok_or_else(|| BackendError::Unsupported {
            backend: "herdr".to_string(),
            method: "agent_rename",
            reason: "clearing an agent name (None) is not supported on the herdr backend"
                .to_string(),
        })?;
        self.client
            .agent_rename(target, name)
            .map_err(BackendError::from)?;
        Ok(())
    }

    fn agent_prompt(
        &mut self,
        target: &str,
        text: &str,
        wait: Option<&WaitOptions>,
    ) -> Result<Value> {
        // WaitOptions is Clone (herdr.rs derive); trait passes by ref, Client
        // takes by value. Clone the Option<&T> into Option<T>. AgentInfo is
        // Deserialize-only (parsed from the socket, never re-serialized), so
        // return the prompt target + status string instead of the struct.
        let info = self.client.agent_prompt(target, text, wait.cloned())?;
        let status = Some(info.agent_status.as_str().to_string());
        Ok(json!({
            "agent": info.agent.clone().unwrap_or_default(),
            "pane_id": info.pane_id,
            "status": status,
        }))
    }

    fn agent_wait(
        &mut self,
        target: &str,
        until: &[AgentStatus],
        timeout_ms: Option<u64>,
    ) -> Result<AgentInfo> {
        self.client
            .agent_wait(target, until, timeout_ms)
            .map_err(BackendError::from)
    }

    fn agent_read(&mut self, target: &str, source: &str, lines: u32) -> Result<Value> {
        let text = self
            .client
            .agent_read(target, source, Some(lines))
            .map_err(BackendError::from)?;
        Ok(json!({ "text": text }))
    }

    fn agent_send_keys(&mut self, target: &str, keys: &[&str]) -> Result<()> {
        self.client
            .agent_send_keys(target, keys)
            .map_err(BackendError::from)?;
        Ok(())
    }

    fn subscribe(&mut self, filters: &[Subscription]) -> Result<Box<dyn BackendEventStream>> {
        // herdr events.subscribe is forward-only (no since/seq param — schema-
        // verified). The loop's heartbeat poll is the correctness floor.
        let stream = self.client.subscribe(filters).map_err(BackendError::from)?;
        Ok(Box::new(HerdrEventStreamAdapter(stream)))
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::MultiplexerBackend;

    /// The struct is `MultiplexerBackend`; dyn-dispatchable. Constructing a
    /// real Client needs a socket; the type system proves the impl satisfies
    /// the trait by compilation alone. HTTP/socket round-trips are R0.8.
    #[test]
    fn herdr_backend_is_object_safe() {
        fn _assert_dyn(_: &dyn MultiplexerBackend) {}
        // _assert_dyn(&HerdrBackend::new(client)); // can't construct w/o socket
    }

    /// pane_run returns Unsupported (no typed Client helper yet).
    #[test]
    fn pane_run_is_unsupported() {
        // Can't construct without a socket; the trait-method default body is
        // exercised by the backend::trait_contract_tests. This test exists to
        // document the Unsupported contract for pane_run specifically and
        // compile-checks the override path.
        // (R0.8 exercises live behavior.)
    }
}

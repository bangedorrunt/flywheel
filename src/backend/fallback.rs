//! Multiplexer fallback: rex first, herdr on transport failure.
//!
//! governed-by: ADR-0002
//!
//! `FallbackBackend` tries the primary backend and retries the same call on
//! the fallback when the primary fails with a transport code (`Connect`,
//! `Io`, `Timeout`). Every other code (semantic rejects, malformed replies,
//! unsupported methods) surfaces loud: falling back there would mask a real
//! gap as a working call. `name()` is `"auto"`; janitors matching `"herdr"`
//! exactly (`pane_keep`) skip auto, which is the safe direction (never
//! delete a binding the fallback may be serving through herdr).

use serde_json::Value;
use toron_core::backend::herdr::{AgentInfo, PaneInfo, Subscription, WaitOptions, WorkspaceInfo};
use toron_core::backend::{
    AgentStatus, BackendError, BackendErrorCode, EventStream, PromptLanding,
};

use crate::backend::MultiplexerBackend;

/// Rex first, herdr when rex is unreachable. The fallback is fixed at
/// construction; a missing fallback (herdr daemon down at startup) leaves a
/// primary-only backend whose transport errors surface unchanged.
pub struct FallbackBackend {
    primary: Box<dyn MultiplexerBackend>,
    fallback: Option<Box<dyn MultiplexerBackend>>,
}

impl FallbackBackend {
    pub fn new(
        primary: Box<dyn MultiplexerBackend>,
        fallback: Option<Box<dyn MultiplexerBackend>>,
    ) -> Self {
        Self { primary, fallback }
    }

    /// One call against the primary, retried on the fallback for transport
    /// failures only. The closure runs at most twice; a fallback transport
    /// failure surfaces as the fallback's own error.
    fn run<T>(
        &self,
        op: impl Fn(&dyn MultiplexerBackend) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        match op(&*self.primary) {
            Err(e) if is_transport(&e) => match &self.fallback {
                Some(fallback) => op(&**fallback),
                None => Err(e),
            },
            other => other,
        }
    }
}

/// Transport failures cross a backend boundary; everything else is a verdict
/// about the call itself. Classify on `code`, never the message.
fn is_transport(e: &BackendError) -> bool {
    matches!(
        e.code(),
        BackendErrorCode::Connect | BackendErrorCode::Io | BackendErrorCode::Timeout
    )
}

impl MultiplexerBackend for FallbackBackend {
    fn name(&self) -> &'static str {
        "auto"
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, BackendError> {
        self.run(|b| b.workspace_list())
    }

    fn workspace_create(&self, label: Option<&str>) -> Result<WorkspaceInfo, BackendError> {
        self.run(|b| b.workspace_create(label))
    }

    fn workspace_close(&self, workspace_id: &str, close_group: bool) -> Result<(), BackendError> {
        self.run(|b| b.workspace_close(workspace_id, close_group))
    }

    fn pane_split(
        &self,
        parent_pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<PaneInfo, BackendError> {
        self.run(|b| b.pane_split(parent_pane_id, direction, cwd))
    }

    fn pane_list(&self, workspace_id: Option<&str>) -> Result<Vec<PaneInfo>, BackendError> {
        self.run(|b| b.pane_list(workspace_id))
    }

    fn pane_close(&self, pane_id: &str) -> Result<(), BackendError> {
        self.run(|b| b.pane_close(pane_id))
    }

    fn pane_run(&self, pane_id: &str, command: &str) -> Result<(), BackendError> {
        self.run(|b| b.pane_run(pane_id, command))
    }

    fn pane_wait_output(
        &self,
        pane_id: &str,
        needle: &str,
        timeout_ms: u64,
    ) -> Result<Value, BackendError> {
        self.run(|b| b.pane_wait_output(pane_id, needle, timeout_ms))
    }

    fn agent_list(&self) -> Result<Vec<AgentInfo>, BackendError> {
        self.run(|b| b.agent_list())
    }

    fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> Result<AgentInfo, BackendError> {
        self.run(|b| b.agent_start(name, kind, pane_id, args))
    }

    fn agent_rename(&self, target: &str, new_name: Option<&str>) -> Result<(), BackendError> {
        self.run(|b| b.agent_rename(target, new_name))
    }

    fn prompt(
        &self,
        target: &str,
        text: &str,
        wait: Option<&WaitOptions>,
    ) -> Result<Value, BackendError> {
        self.run(|b| b.prompt(target, text, wait))
    }

    fn prompt_coded(
        &self,
        target: &str,
        text: &str,
        wait: Option<&WaitOptions>,
    ) -> Result<PromptLanding, BackendError> {
        self.run(|b| b.prompt_coded(target, text, wait))
    }

    fn agent_wait(
        &self,
        target: &str,
        until: &[AgentStatus],
        timeout_ms: Option<u64>,
    ) -> Result<AgentInfo, BackendError> {
        self.run(|b| b.agent_wait(target, until, timeout_ms))
    }

    fn agent_read(&self, target: &str, source: &str, lines: u32) -> Result<Value, BackendError> {
        self.run(|b| b.agent_read(target, source, lines))
    }

    fn agent_send_keys(&self, target: &str, keys: &[&str]) -> Result<(), BackendError> {
        self.run(|b| b.agent_send_keys(target, keys))
    }

    fn subscribe(&self, filters: &[Subscription]) -> Result<Box<dyn EventStream>, BackendError> {
        self.run(|b| b.subscribe(filters))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted backend: every call fails with the scripted code, except
    /// the canned workspace list a "reached" fallback answers.
    struct Scripted {
        name: &'static str,
        code: BackendErrorCode,
        answer: Vec<WorkspaceInfo>,
    }

    impl Scripted {
        fn fail(code: BackendErrorCode) -> Self {
            Self {
                name: "scripted",
                code,
                answer: Vec::new(),
            }
        }

        fn err(&self, method: &str) -> BackendError {
            BackendError::new(self.code.clone(), format!("scripted {method}"))
        }
    }

    impl MultiplexerBackend for Scripted {
        fn name(&self) -> &'static str {
            self.name
        }

        fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, BackendError> {
            if self.answer.is_empty() {
                Err(self.err("workspace_list"))
            } else {
                Ok(self.answer.clone())
            }
        }
    }

    fn workspace(id: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.to_string(),
            number: 1,
            label: "work".to_string(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: String::new(),
            agent_status: AgentStatus::Unknown,
            cwd: None,
        }
    }

    #[test]
    fn transport_failures_reach_the_fallback() {
        for code in [
            BackendErrorCode::Connect,
            BackendErrorCode::Io,
            BackendErrorCode::Timeout,
        ] {
            let backend = FallbackBackend::new(
                Box::new(Scripted::fail(code.clone())),
                Some(Box::new(Scripted {
                    name: "fallback",
                    code: BackendErrorCode::Other("unused".into()),
                    answer: vec![workspace("session:fake-1")],
                })),
            );
            let list = backend.workspace_list().expect("fallback answers");
            assert_eq!(list.len(), 1, "{code:?} must fall back");
            assert_eq!(list[0].workspace_id, "session:fake-1");
        }
    }

    #[test]
    fn semantic_rejects_never_reach_the_fallback() {
        for code in [
            BackendErrorCode::AgentBlocked,
            BackendErrorCode::Malformed,
            BackendErrorCode::Unsupported,
            BackendErrorCode::Other("rejected".into()),
        ] {
            // The fallback answers if called; the primary's verdict must win.
            let backend = FallbackBackend::new(
                Box::new(Scripted::fail(code.clone())),
                Some(Box::new(Scripted {
                    name: "fallback",
                    code: BackendErrorCode::Other("unused".into()),
                    answer: vec![workspace("session:fake-1")],
                })),
            );
            let err = backend.workspace_list().expect_err("verdict surfaces");
            assert_eq!(err.code(), &code, "{code:?} must not fall back");
        }
    }

    #[test]
    fn a_missing_fallback_surfaces_the_primary_error() {
        let backend = FallbackBackend::new(Box::new(Scripted::fail(BackendErrorCode::Io)), None);
        let err = backend.workspace_list().expect_err("no fallback");
        assert_eq!(err.code(), &BackendErrorCode::Io);
        assert_eq!(backend.name(), "auto");
    }
}

//! superlogical multiplexer backend over the `rex` CLI.
//!
//! governed-by: ADR-0002  ·  implements: `MultiplexerBackend`
//!
//! Session id is the workspace id. Block id is the pane id. The process
//! and the status fold live in `rex_cli` (the fold is `rex_cli::rex_map`).
//! This file only chooses the rex command. It never calls herdr.
//!
//! `src/backend/mod.rs` and `src/backend/rex_map.rs` are exclusively leased
//! by PurpleGorge for bd-i328, so this module declares `rex_cli` itself.
//!
//! `subscribe`, `agent_wait`, and `agent_send_keys` stay on the trait
//! default, which names the method. A prompt's `wait` is not polled: that
//! would be `agent_wait`.
//!
//! gated by: cargo feature `backend-superlogical` (off by default).
//! The host box is the sync `Backend` `Result`. No elm-effect program.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use toron_core::backend::herdr::{AgentInfo, PaneInfo, Subscription, WaitOptions, WorkspaceInfo};
use toron_core::backend::{AgentStatus, BackendError, EventStream, PromptLanding};

#[path = "rex_cli.rs"]
mod rex_cli;

use crate::backend::MultiplexerBackend;
use rex_cli::rex_map::{capture_format, split_direction, status_from_program_status, tail_lines};
use rex_cli::{
    Rex, RexBlock, RexEventStream, RexFail, RexSession, block_close_args, capture_args,
    events_args, list_blocks_args, ls_args, new_args, parse_blocks, parse_created_block_id,
    parse_created_session, parse_sessions, program_status_args, rename_args, run_args,
    send_key_args, session_kill_args, split_args, write_args,
};

/// Superlogical backend over the `rex` CLI.
pub struct SuperlogicalBackend;

impl SuperlogicalBackend {
    fn rex() -> Result<Rex, toron_core::backend::BackendError> {
        Rex::resolve().map_err(RexFail::into_backend)
    }

    fn locate(rex: &Rex, target: &str) -> Result<(RexSession, RexBlock), RexFail> {
        if target.is_empty() {
            return Err(RexFail::Rejected {
                message: "rex block id is empty".into(),
            });
        }
        let sessions = parse_sessions(&rex.json(&ls_args())?)?;
        let mut by_id = Vec::new();
        let mut by_label = Vec::new();
        for session in sessions {
            let blocks = parse_blocks(&rex.json(&list_blocks_args(&session.session_id))?)?;
            for block in blocks {
                if block.block_id == target {
                    by_id.push((session.clone(), block));
                } else if !block.label.is_empty() && block.label == target {
                    by_label.push((session.clone(), block));
                }
            }
        }
        if by_id.len() == 1 {
            return Ok(by_id.remove(0));
        }
        if by_id.len() > 1 {
            return Err(RexFail::Rejected {
                message: format!("rex block {target} is in {} sessions", by_id.len()),
            });
        }
        if by_label.len() == 1 {
            return Ok(by_label.remove(0));
        }
        if by_label.len() > 1 {
            return Err(RexFail::Rejected {
                message: format!("rex label {target} is in {} sessions", by_label.len()),
            });
        }
        Err(RexFail::Rejected {
            message: format!("rex block {target} not found"),
        })
    }

    fn blocks(rex: &Rex, session: &str) -> Result<Vec<RexBlock>, RexFail> {
        parse_blocks(&rex.json(&list_blocks_args(session))?)
    }

    fn block_status(rex: &Rex, session: &str, block: &str) -> Result<AgentStatus, RexFail> {
        let value = rex.json(&program_status_args(session, block))?;
        Ok(status_from_program_status(&value))
    }
}

impl MultiplexerBackend for SuperlogicalBackend {
    fn name(&self) -> &'static str {
        "superlogical"
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let sessions = parse_sessions(&rex.json(&ls_args()).map_err(RexFail::into_backend)?)
            .map_err(RexFail::into_backend)?;
        Ok(sessions
            .into_iter()
            .enumerate()
            .map(|(index, session)| workspace_from(session, index))
            .collect())
    }

    fn workspace_create(
        &self,
        label: Option<&str>,
    ) -> Result<WorkspaceInfo, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let value = rex.json(&new_args(label)).map_err(RexFail::into_backend)?;
        let session = parse_created_session(&value, label).map_err(RexFail::into_backend)?;
        Ok(workspace_from(session, 0))
    }

    fn pane_list(
        &self,
        workspace_id: Option<&str>,
    ) -> Result<Vec<PaneInfo>, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let sessions = match workspace_id {
            Some(id) => vec![id.to_string()],
            None => parse_sessions(&rex.json(&ls_args()).map_err(RexFail::into_backend)?)
                .map_err(RexFail::into_backend)?
                .into_iter()
                .map(|session| session.session_id)
                .collect(),
        };
        let mut panes = Vec::new();
        for session in sessions {
            let blocks = Self::blocks(&rex, &session).map_err(RexFail::into_backend)?;
            for block in blocks {
                panes.push(pane_from(&block, &session, AgentStatus::Unknown));
            }
        }
        Ok(panes)
    }

    fn pane_split(
        &self,
        parent_pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<PaneInfo, toron_core::backend::BackendError> {
        let Some(direction) = split_direction(direction) else {
            return Err(RexFail::Rejected {
                message: format!(
                    "rex split direction {direction:?} is not left, right, above, or below"
                ),
            }
            .into_backend());
        };
        let rex = Self::rex()?;
        let (session, parent) =
            Self::locate(&rex, parent_pane_id).map_err(RexFail::into_backend)?;
        let value = rex
            .json(&split_args(
                &session.session_id,
                &parent.block_id,
                direction,
                cwd,
            ))
            .map_err(RexFail::into_backend)?;
        let id = parse_created_block_id(&value).map_err(RexFail::into_backend)?;
        let window = value
            .get("window_id")
            .and_then(Value::as_str)
            .unwrap_or(parent.window_id.as_str());
        Ok(pane_from(
            &RexBlock {
                block_id: id,
                label: String::new(),
                creator_name: String::new(),
                window_id: window.to_string(),
            },
            &session.session_id,
            AgentStatus::Unknown,
        ))
    }

    fn pane_close(&self, pane_id: &str) -> Result<(), toron_core::backend::BackendError> {
        // One block. `block.close` does not destroy the session.
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, pane_id).map_err(RexFail::into_backend)?;
        rex.text(&block_close_args(&session.session_id, &block.block_id))
            .map_err(RexFail::into_backend)?;
        Ok(())
    }

    fn pane_run(
        &self,
        pane_id: &str,
        command: &str,
    ) -> Result<(), toron_core::backend::BackendError> {
        if command.is_empty() {
            return Err(RexFail::Rejected {
                message: "rex run command is empty".into(),
            }
            .into_backend());
        }
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, pane_id).map_err(RexFail::into_backend)?;
        rex.text(&run_args(&session.session_id, &block.block_id, command))
            .map_err(RexFail::into_backend)?;
        Ok(())
    }

    fn agent_list(&self) -> Result<Vec<AgentInfo>, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let sessions = parse_sessions(&rex.json(&ls_args()).map_err(RexFail::into_backend)?)
            .map_err(RexFail::into_backend)?;
        let mut agents = Vec::new();
        for session in sessions {
            let blocks = Self::blocks(&rex, &session.session_id).map_err(RexFail::into_backend)?;
            for block in blocks {
                let status = Self::block_status(&rex, &session.session_id, &block.block_id)
                    .map_err(RexFail::into_backend)?;
                agents.push(agent_from(&block, &session.session_id, status));
            }
        }
        Ok(agents)
    }

    fn prompt(
        &self,
        target: &str,
        text: &str,
        _wait: Option<&WaitOptions>,
    ) -> Result<Value, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, target).map_err(RexFail::into_backend)?;
        rex.json(&write_args(&session.session_id, &block.block_id, text))
            .map_err(RexFail::into_backend)
    }

    fn agent_read(
        &self,
        target: &str,
        source: &str,
        lines: u32,
    ) -> Result<Value, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, target).map_err(RexFail::into_backend)?;
        let text = rex
            .text(&capture_args(
                &session.session_id,
                &block.block_id,
                capture_format(source),
            ))
            .map_err(RexFail::into_backend)?;
        Ok(json!({ "text": tail_lines(&text, lines) }))
    }

    fn workspace_close(
        &self,
        workspace_id: &str,
        _close_group: bool,
    ) -> Result<(), toron_core::backend::BackendError> {
        // A rex session is the workspace. Both close flags destroy it.
        // `session kill` does not fall through to herdr.
        let rex = Self::rex()?;
        rex.text(&session_kill_args(workspace_id))
            .map_err(RexFail::into_backend)?;
        Ok(())
    }

    fn agent_rename(
        &self,
        target: &str,
        new_name: Option<&str>,
    ) -> Result<(), toron_core::backend::BackendError> {
        let Some(label) = new_name.filter(|name| !name.is_empty()) else {
            return Err(RexFail::Rejected {
                message: "rex rename needs a label".into(),
            }
            .into_backend());
        };
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, target).map_err(RexFail::into_backend)?;
        rex.text(&rename_args(
            &session.session_id,
            &block.block_id,
            label,
        ))
        .map_err(RexFail::into_backend)?;
        Ok(())
    }

    fn agent_send_keys(
        &self,
        target: &str,
        keys: &[&str],
    ) -> Result<(), toron_core::backend::BackendError> {
        if keys.is_empty() {
            return Ok(());
        }
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, target).map_err(RexFail::into_backend)?;
        rex.text(&send_key_args(
            &session.session_id,
            &block.block_id,
            keys,
        ))
        .map_err(RexFail::into_backend)?;
        Ok(())
    }

    fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> Result<AgentInfo, toron_core::backend::BackendError> {
        let mut command = kind.to_string();
        for arg in args {
            command.push(' ');
            command.push_str(arg);
        }
        self.pane_run(pane_id, &command)?;
        if !name.is_empty() {
            self.agent_rename(pane_id, Some(name))?;
        }
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, pane_id).map_err(RexFail::into_backend)?;
        let status = Self::block_status(&rex, &session.session_id, &block.block_id)
            .unwrap_or(AgentStatus::Unknown);
        let mut agent = agent_from(&block, &session.session_id, status);
        if !name.is_empty() {
            agent.name = Some(name.to_string());
        }
        Ok(agent)
    }

    fn agent_wait(
        &self,
        target: &str,
        until: &[AgentStatus],
        timeout_ms: Option<u64>,
    ) -> Result<AgentInfo, toron_core::backend::BackendError> {
        // No timeout means one read. An open wait would never return.
        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        loop {
            let rex = Self::rex()?;
            let (session, block) = Self::locate(&rex, target).map_err(RexFail::into_backend)?;
            let status = Self::block_status(&rex, &session.session_id, &block.block_id)
                .map_err(RexFail::into_backend)?;
            if until.is_empty() || until.contains(&status) {
                return Ok(agent_from(&block, &session.session_id, status));
            }
            match deadline {
                None => {
                    return Err(BackendError::timeout(
                        "rex status did not match and no timeout was set",
                    ));
                }
                Some(deadline) if Instant::now() >= deadline => {
                    return Err(BackendError::timeout(format!(
                        "rex status {status:?} did not become {until:?}"
                    )));
                }
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(Duration::from_millis(20).min(left));
                }
            }
        }
    }

    fn prompt_coded(
        &self,
        target: &str,
        text: &str,
        wait: Option<&WaitOptions>,
    ) -> Result<PromptLanding, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let (session, block) = Self::locate(&rex, target).map_err(RexFail::into_backend)?;
        let status = Self::block_status(&rex, &session.session_id, &block.block_id)
            .map_err(RexFail::into_backend)?;
        if status == AgentStatus::Blocked {
            return Ok(PromptLanding {
                status: None,
                code: Some("agent_blocked".into()),
            });
        }
        self.prompt(target, text, wait)?;
        if let Some(wait) = wait {
            let agent = self.agent_wait(target, &wait.until, wait.timeout_ms)?;
            return Ok(PromptLanding {
                status: Some(agent.agent_status),
                code: None,
            });
        }
        Ok(PromptLanding {
            status: Some(status),
            code: None,
        })
    }

    fn pane_wait_output(
        &self,
        pane_id: &str,
        needle: &str,
        timeout_ms: u64,
    ) -> Result<Value, toron_core::backend::BackendError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let value = self.agent_read(pane_id, "text", 0)?;
            let text = value.get("text").and_then(Value::as_str).unwrap_or("");
            if text.contains(needle) {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                return Err(BackendError::timeout(format!(
                    "rex output did not contain {needle:?}"
                )));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(Duration::from_millis(20).min(left));
        }
    }

    fn subscribe(
        &self,
        filters: &[Subscription],
    ) -> Result<Box<dyn EventStream>, toron_core::backend::BackendError> {
        let rex = Self::rex()?;
        let panes: Vec<String> = filters
            .iter()
            .map(|filter| filter.pane_id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        let child = rex
            .spawn(&events_args(None))
            .map_err(RexFail::into_backend)?;
        let stream = RexEventStream::start(child, panes).map_err(RexFail::into_backend)?;
        Ok(Box::new(stream))
    }
}

fn workspace_from(session: RexSession, index: usize) -> WorkspaceInfo {
    WorkspaceInfo {
        workspace_id: session.session_id,
        number: index as u64 + 1,
        label: session.label,
        focused: session.focused,
        pane_count: session.pane_count,
        tab_count: session.tab_count,
        active_tab_id: session.active_tab_id,
        agent_status: AgentStatus::Unknown,
        cwd: session.cwd,
    }
}

fn pane_from(block: &RexBlock, session: &str, status: AgentStatus) -> PaneInfo {
    PaneInfo {
        pane_id: block.block_id.clone(),
        terminal_id: String::new(),
        workspace_id: session.to_string(),
        tab_id: block.window_id.clone(),
        focused: false,
        agent_status: status,
        revision: 0,
        title: none_if_empty(&block.label),
        cwd: None,
        agent: none_if_empty(&block.creator_name),
        agent_session: None,
        display_agent: none_if_empty(&block.creator_name),
        foreground_cwd: None,
        label: none_if_empty(&block.label),
        scroll: None,
        state_labels: serde_json::Map::new(),
        terminal_title: none_if_empty(&block.label),
        terminal_title_stripped: none_if_empty(&block.label),
        tokens: serde_json::Map::new(),
    }
}

fn agent_from(block: &RexBlock, session: &str, status: AgentStatus) -> AgentInfo {
    AgentInfo {
        agent: none_if_empty(&block.creator_name),
        agent_status: status,
        cwd: None,
        display_agent: none_if_empty(&block.label).or_else(|| none_if_empty(&block.creator_name)),
        focused: false,
        foreground_cwd: None,
        interactive_ready: status == AgentStatus::Idle,
        launch_pending: false,
        name: none_if_empty(&block.label),
        pane_id: block.block_id.clone(),
        revision: 0,
        state_change_seq: 0,
        completion_seq: None,
        state_labels: serde_json::Map::new(),
        tab_id: block.window_id.clone(),
        terminal_id: String::new(),
        terminal_title: none_if_empty(&block.label),
        workspace_id: session.to_string(),
    }
}

fn none_if_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::rex_cli::RexBinGuard;
    use super::*;
    use toron_core::backend::BackendErrorCode;

    const FAKE_REX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> @@LOG@@
for arg in "$@"; do
  case "$arg" in
    *herdr*) echo "refusing herdr" >&2; exit 3 ;;
  esac
done
cmd=""
for arg in "$@"; do
  case "$arg" in
    ls|api|block|new|split|run|capture) cmd=$arg; break ;;
  esac
done
case "$cmd" in
  ls)
    printf '%s\n' '{"sessions":[{"session_id":"session:fake-1","label":"work","pane_count":1,"tab_count":1}]}'
    ;;
  api)
    printf '%s\n' '{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"build","creator_name":"com.superlogical.terminal"}]}'
    ;;
  block)
    printf '%s\n' '{"records":[{"state":"working"}]}'
    ;;
  *)
    echo "unexpected $*" >&2
    exit 1
    ;;
esac
"#;

    #[test]
    fn superlogical_lists_a_session_from_a_fake_rex() {
        let guard = RexBinGuard::install(FAKE_REX);
        let backend = SuperlogicalBackend;
        let list = backend
            .workspace_list()
            .expect("workspace_list over fake rex");
        assert_eq!(list.len(), 1, "one session, one workspace: {list:#?}");
        let workspace = &list[0];
        assert_eq!(workspace.workspace_id, "session:fake-1");
        assert_eq!(workspace.label, "work");
        assert_eq!(workspace.pane_count, 1);
        assert_eq!(workspace.tab_count, 1);
        assert_eq!(backend.name(), "superlogical");

        let panes = backend
            .pane_list(Some("session:fake-1"))
            .expect("pane_list");
        assert_eq!(panes.len(), 1, "one block, one pane: {panes:#?}");
        assert_eq!(panes[0].pane_id, "block:fake-1");
        assert_eq!(panes[0].tab_id, "window:fake-1");
        assert_eq!(
            panes[0].agent_status,
            AgentStatus::Unknown,
            "pane_list does not read program_status"
        );

        let agents = backend.agent_list().expect("agent_list");
        assert_eq!(agents.len(), 1, "{agents:#?}");
        assert_eq!(agents[0].pane_id, "block:fake-1");
        assert_eq!(agents[0].workspace_id, "session:fake-1");
        assert_eq!(agents[0].agent_status, AgentStatus::Working);

        let log = guard.log_text();
        assert!(log.contains("ls"), "{log}");
        assert!(log.contains("--json"), "{log}");
        assert!(log.contains("session.list_blocks"), "{log}");
        assert!(log.contains("program_status"), "{log}");
        assert!(!log.contains("herdr"), "{log}");
        assert!(!log.contains("session.close"), "{log}");
    }

    #[test]
    fn superlogical_without_rex_names_the_install_not_a_release() {
        let _guard = RexBinGuard::empty();
        let err = SuperlogicalBackend
            .workspace_list()
            .expect_err("missing rex");
        assert_eq!(err.code(), &BackendErrorCode::Io);
        assert!(
            err.to_string().contains("rex CLI not installed"),
            "names the fix: {err}"
        );
        assert!(
            !err.to_string().contains("not yet released"),
            "never a release excuse: {err}"
        );
    }

    const PARITY_REX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> @@LOG@@
cmd=""
for arg in "$@"; do
  case "$arg" in
    ls|session|block|events|capture|api|run) cmd=$arg; break ;;
  esac
done
sub=""
for arg in "$@"; do
  case "$arg" in
    kill|rename|send-key|program_status|write) sub=$arg ;;
    *list_blocks*) sub=list_blocks ;;
  esac
done
case "$cmd:$sub" in
  session:kill) echo killed; exit 0 ;;
  block:send-key) echo keyed; exit 0 ;;
  block:rename) echo renamed; exit 0 ;;
  events:)
    printf '%s\n' '{"type":"session_event","payload":{"event":{"type":"terminal.program_status_changed","payload":{"block_id":"block:fake-1","record":{"state":"idle"}}}}}'
    exit 0
    ;;
  block:program_status)
    printf '%s\n' '{"records":[{"state":"idle"}]}'
    exit 0
    ;;
  capture:)
    printf '%s\n' 'ready now'
    exit 0
    ;;
  api:list_blocks)
    printf '%s\n' '{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"build","creator_name":"com.superlogical.terminal"}]}'
    exit 0
    ;;
  ls:)
    printf '%s\n' '{"sessions":[{"session_id":"session:fake-1","label":"work","pane_count":1,"tab_count":1}]}'
    exit 0
    ;;
  *)
    echo ok
    exit 0
    ;;
esac
"#;

    #[test]
    fn superlogical_matches_herdr_lifecycle_on_a_fake_rex() {
        let guard = RexBinGuard::install(PARITY_REX);
        let backend = SuperlogicalBackend;
        backend
            .workspace_close("session:fake-1", false)
            .expect("session kill");
        backend
            .agent_rename("block:fake-1", Some("dusty"))
            .expect("rename");
        backend
            .agent_send_keys("block:fake-1", &["enter"])
            .expect("send-key");
        let started = backend
            .agent_start("dusty", "opencode", "block:fake-1", &["run".into()])
            .expect("start");
        assert_eq!(started.name.as_deref(), Some("dusty"));
        let waited = backend
            .agent_wait("block:fake-1", &[AgentStatus::Idle], Some(1_000))
            .expect("already idle");
        assert_eq!(waited.agent_status, AgentStatus::Idle);
        let screen = backend
            .pane_wait_output("block:fake-1", "ready", 1_000)
            .expect("needle");
        assert!(screen["text"].as_str().unwrap_or("").contains("ready"));
        let mut stream = backend.subscribe(&[]).expect("events");
        let event = stream.read_event().expect("line").expect("one event");
        match event {
            toron_core::backend::BackendEvent::AgentStatusChanged { pane, status, .. } => {
                assert_eq!(pane.as_str(), "block:fake-1");
                assert_eq!(status, AgentStatus::Idle);
            }
            other => panic!("status event, got {other:?}"),
        }
        let log = guard.log_text();
        assert!(log.contains("session kill"), "{log}");
        assert!(log.contains("send-key"), "{log}");
        assert!(log.contains("rename"), "{log}");
        assert!(!log.contains("herdr"), "{log}");
    }

    #[test]
    fn superlogical_does_not_type_into_a_blocked_program() {
        let script = r#"#!/bin/sh
printf '%s\n' "$*" >> @@LOG@@
cmd=""
for arg in "$@"; do
  case "$arg" in
    ls|api|block) cmd=$arg; break ;;
  esac
done
sub=""
for arg in "$@"; do
  case "$arg" in
    program_status|write) sub=$arg ;;
    *list_blocks*) sub=list_blocks ;;
  esac
done
case "$cmd:$sub" in
  block:program_status)
    printf '%s\n' '{"records":[{"state":"blocked","kind":"permission"}]}'
    ;;
  api:list_blocks)
    printf '%s\n' '{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"build"}]}'
    ;;
  ls:)
    printf '%s\n' '{"sessions":[{"session_id":"session:fake-1","label":"work"}]}'
    ;;
  block:write)
    echo wrote-blocked >> @@LOG@@
    echo '{}'
    ;;
  *) echo ok ;;
esac
"#;
        let guard = RexBinGuard::install(script);
        let landing = SuperlogicalBackend
            .prompt_coded("block:fake-1", "go", None)
            .expect("coded prompt");
        assert_eq!(landing.status, None);
        assert_eq!(landing.code.as_deref(), Some("agent_blocked"));
        let log = guard.log_text();
        assert!(!log.contains("wrote-blocked"), "{log}");
        assert!(!log.contains(" write"), "{log}");
    }

    #[test]
    fn superlogical_wait_times_out_without_sleeping_when_the_budget_is_zero() {
        let guard = RexBinGuard::install(FAKE_REX);
        let err = SuperlogicalBackend
            .agent_wait("block:fake-1", &[AgentStatus::Idle], Some(0))
            .expect_err("working is not idle");
        assert_eq!(err.code(), &BackendErrorCode::Timeout, "{err}");
        let _ = guard;
    }

    const PARITY_REMAINING_REX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> @@LOG@@
cmd=""
for arg in "$@"; do
  case "$arg" in
    ls|session|block|capture|api|run|split|new) cmd=$arg; break ;;
  esac
done
sub=""
for arg in "$@"; do
  case "$arg" in
    kill|rename|send-key|program_status|write) sub=$arg ;;
    *list_blocks*) sub=list_blocks ;;
  esac
done
case "$cmd:$sub" in
  ls:)
    printf '%s\n' '{"sessions":[{"session_id":"session:fake-1","label":"work","pane_count":1,"tab_count":1}]}'
    ;;
  api:list_blocks)
    printf '%s\n' '{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"build","creator_name":"com.superlogical.terminal"}]}'
    ;;
  block:program_status)
    printf '%s\n' '{"records":[{"state":"working"}]}'
    ;;
  block:write)
    printf '%s\n' '{}'
    ;;
  capture:)
    printf '%s\n' 'one' 'two' 'three'
    ;;
  split:)
    printf '%s\n' '{"block_id":"block:fake-2","window_id":"window:fake-1"}'
    ;;
  run:)
    printf '%s\n' '{}'
    ;;
  new:)
    printf '%s\n' '{"session_id":"session:fake-2"}'
    ;;
  *)
    echo "unexpected $*" >&2
    exit 1
    ;;
esac
"#;

    /// The remaining trait methods go through rex with no herdr contact:
    /// create, split, run, read, prompt, and the landed prompt_coded path.
    #[test]
    fn superlogical_covers_create_split_run_read_and_prompt() {
        // Two lock domains touch process-global `REX_BIN_DIR`: `RexBinGuard`
        // (`REX_LOCK`) and `config::lock_env` (the factory feature-off test
        // sets the
        // same var). Hold both so an interleaved empty-dir set cannot fail a
        // mid-test locate with "not installed".
        let _env = crate::config::lock_env();
        let guard = RexBinGuard::install(PARITY_REMAINING_REX);
        let backend = SuperlogicalBackend;

        let workspace = backend
            .workspace_create(Some("work2"))
            .expect("session new");
        assert_eq!(workspace.workspace_id, "session:fake-2");
        assert_eq!(workspace.label, "work2");

        let pane = backend
            .pane_split("block:fake-1", "right", None)
            .expect("split");
        assert_eq!(pane.pane_id, "block:fake-2");
        assert_eq!(pane.tab_id, "window:fake-1");
        assert_eq!(pane.workspace_id, "session:fake-1");

        backend.pane_run("block:fake-1", "echo hi").expect("run");
        let err = backend
            .pane_run("block:fake-1", "")
            .expect_err("empty command is refused before spawn");
        assert!(err.to_string().contains("empty"), "{err}");

        let screen = backend
            .agent_read("block:fake-1", "text", 2)
            .expect("capture");
        assert_eq!(screen["text"].as_str().unwrap_or(""), "two\nthree\n");

        backend.prompt("block:fake-1", "hello", None).expect("write");
        let landed = backend
            .prompt_coded("block:fake-1", "go", None)
            .expect("coded prompt");
        assert_eq!(landed.status, Some(AgentStatus::Working));
        assert_eq!(landed.code, None);

        let err = backend
            .agent_rename("block:fake-1", Some(""))
            .expect_err("rename needs a label");
        assert!(err.to_string().contains("label"), "{err}");
        let err = backend
            .pane_split("block:fake-1", "diagonal", None)
            .expect_err("unknown direction is refused before spawn");
        assert!(err.to_string().contains("left, right, above"), "{err}");

        let log = guard.log_text();
        for verb in ["new", "split", "run", "capture", "write", "program_status"] {
            assert!(log.contains(verb), "{log}");
        }
        assert!(!log.contains("herdr"), "{log}");
    }
}

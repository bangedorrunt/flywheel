//! superlogical multiplexer backend — the `rex` CLI (ADR-0002 R2).
//!
//! governed-by: ADR-0002  ·  implements: `MultiplexerBackend`
//!
//! Superlogical's multiplexer is driven by `rex` (session = workspace, window
//! = tab, block = pane). Every call here shells out to `rex`; no herdr call is
//! ever made, and a machine without `rex` gets `Unsupported` naming the
//! install, never a silent fallback.
//!
//! Left Unsupported on purpose (`agent_wait` needs a live event stream to be
//! more than a poll): `subscribe`, `agent_wait`, `agent_send_keys`. The trait
//! defaults return those with the backend + method named.
//!
//! gated by: cargo feature `backend-superlogical` (off by default).

use serde_json::Value;
use toron_core::backend::herdr::{AgentInfo, PaneInfo, WaitOptions, WorkspaceInfo};
use toron_core::backend::{AgentStatus, BackendError, BackendErrorCode};
use toron_core::osc7501::Board;

use crate::backend::MultiplexerBackend;

/// Superlogical backend over the `rex` CLI.
pub struct SuperlogicalBackend;

/// One block row, reduced to what the trait surface needs. `rex` returns more
/// (bounds, owner, layers); ignoring it keeps this a thin adapter.
struct Block {
    id: String,
    window: String,
    label: String,
    creator: Option<String>,
}

impl SuperlogicalBackend {
    /// Run `rex` and return stdout. A missing binary is an install problem,
    /// not a backend gap, so the message says exactly what to do.
    fn rex(&self, args: &[&str]) -> Result<String, BackendError> {
        let out = std::process::Command::new("rex")
            .args(args)
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => BackendError::unsupported(
                    "superlogical: rex CLI not installed (put `rex` on PATH)",
                ),
                _ => {
                    BackendError::new(BackendErrorCode::Io, format!("rex {}: {e}", args.join(" ")))
                }
            })?;
        if !out.status.success() {
            return Err(BackendError::new(
                BackendErrorCode::Other("rex_failed".into()),
                format!(
                    "rex {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn rex_json(&self, args: &[&str]) -> Result<Value, BackendError> {
        let out = self.rex(args)?;
        serde_json::from_str(&out).map_err(|e| {
            BackendError::new(
                BackendErrorCode::Malformed,
                format!("rex {}: {e}: {out}", args.join(" ")),
            )
        })
    }

    /// Sessions (`rex ls --json`). Sessions are workspaces.
    fn sessions(&self) -> Result<Vec<Value>, BackendError> {
        let v = self.rex_json(&["ls", "--json"])?;
        Ok(match v {
            Value::Array(a) => a,
            // Newer servers may wrap the list; accept both shapes.
            other => other
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// One session's blocks through the documented API method (not the CLI
    /// table, whose column layout is presentation).
    fn session_blocks(&self, session: &str) -> Result<Vec<Block>, BackendError> {
        let v = self.rex_json(&["api", "call", "-s", session, "session.list_blocks"])?;
        Ok(v.get("blocks")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(block_from).collect())
            .unwrap_or_default())
    }

    /// Status from the program's own OSC 7501 record, folded by toron-core's
    /// canonical mapping. A block with no record, or one that cannot answer,
    /// reads `Unknown` — it never proves completion.
    fn block_status(&self, block_id: &str) -> AgentStatus {
        let Ok(v) = self.rex_json(&["block", "call", "-b", block_id, "program_status"]) else {
            return AgentStatus::Unknown;
        };
        status_from_program_status(&v)
    }
}

impl MultiplexerBackend for SuperlogicalBackend {
    fn name(&self) -> &'static str {
        "superlogical"
    }

    fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, BackendError> {
        Ok(self
            .sessions()?
            .iter()
            .enumerate()
            .map(|(i, s)| WorkspaceInfo {
                workspace_id: text(s, &["session_id", "id"]).unwrap_or_default(),
                number: i as u64 + 1,
                label: text(s, &["label"]).unwrap_or_default(),
                focused: s.get("focused").and_then(Value::as_bool).unwrap_or(false),
                pane_count: num(s, &["pane_count", "block_count"]).unwrap_or(0),
                tab_count: num(s, &["tab_count", "window_count"]).unwrap_or(0),
                active_tab_id: text(s, &["active_window_id"]).unwrap_or_default(),
                agent_status: AgentStatus::Unknown,
                cwd: text(s, &["cwd"]),
            })
            .collect())
    }

    fn workspace_create(&self, label: Option<&str>) -> Result<WorkspaceInfo, BackendError> {
        let mut args = vec!["new"];
        args.extend(label);
        args.push("--json");
        let v = self.rex_json(&args)?;
        let windows = v
            .get("initial_windows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let panes: u64 = windows
            .iter()
            .map(|w| {
                w.get("block_ids")
                    .and_then(Value::as_array)
                    .map_or(0, |b| b.len() as u64)
            })
            .sum();
        Ok(WorkspaceInfo {
            workspace_id: text(&v, &["session_id", "id"]).unwrap_or_default(),
            number: 1,
            label: label.unwrap_or_default().to_string(),
            focused: true,
            pane_count: panes,
            tab_count: windows.len() as u64,
            active_tab_id: windows
                .first()
                .and_then(|w| text(w, &["window_id"]))
                .unwrap_or_default(),
            agent_status: AgentStatus::Unknown,
            cwd: None,
        })
    }

    fn pane_list(&self, workspace_id: Option<&str>) -> Result<Vec<PaneInfo>, BackendError> {
        let sessions = match workspace_id {
            Some(id) => vec![id.to_string()],
            None => self
                .sessions()?
                .iter()
                .filter_map(|s| text(s, &["session_id", "id"]))
                .collect(),
        };
        let mut panes = Vec::new();
        for session in &sessions {
            for (i, b) in self.session_blocks(session)?.into_iter().enumerate() {
                panes.push(pane_from(&b, session, i as u64, self.block_status(&b.id)));
            }
        }
        Ok(panes)
    }

    fn pane_split(
        &self,
        parent_pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<PaneInfo, BackendError> {
        let split = format!("--split={direction}");
        let mut args = vec!["split", "-b", parent_pane_id, &split, "--json"];
        args.extend(cwd.map(|c| ["-c", c]).unwrap_or_default());
        let v = self.rex_json(&args)?;
        let id = first_block_id(&v).unwrap_or_default();
        Ok(pane_from(
            &Block {
                id: id.clone(),
                window: text(&v, &["window_id"]).unwrap_or_default(),
                label: String::new(),
                creator: None,
            },
            "",
            0,
            self.block_status(&id),
        ))
    }

    fn pane_close(&self, pane_id: &str) -> Result<(), BackendError> {
        // One block, never the session.
        self.rex(&["block", "close", "-b", pane_id]).map(|_| ())
    }

    fn pane_run(&self, pane_id: &str, command: &str) -> Result<(), BackendError> {
        // `rex run` places a NEW block; `-b` + `--split` anchor it in the
        // caller's window instead of a new one. `sh -c` keeps the command's
        // own quoting intact rather than splitting it here.
        self.rex(&[
            "run",
            "-b",
            pane_id,
            "--split=below",
            "--focus=false",
            "--json",
            "--",
            "sh",
            "-c",
            command,
        ])
        .map(|_| ())
    }

    fn agent_list(&self) -> Result<Vec<AgentInfo>, BackendError> {
        Ok(self
            .pane_list(None)?
            .into_iter()
            .map(|p| AgentInfo {
                agent: p.agent,
                agent_status: p.agent_status,
                cwd: p.cwd,
                display_agent: p.display_agent,
                focused: p.focused,
                foreground_cwd: p.foreground_cwd,
                interactive_ready: true,
                launch_pending: false,
                name: p.label.clone(),
                pane_id: p.pane_id,
                revision: p.revision,
                state_change_seq: 0,
                completion_seq: None,
                state_labels: p.state_labels,
                tab_id: p.tab_id,
                terminal_id: p.terminal_id,
                terminal_title: p.terminal_title,
                workspace_id: p.workspace_id,
            })
            .collect())
    }

    fn prompt(
        &self,
        target: &str,
        text: &str,
        _wait: Option<&WaitOptions>,
    ) -> Result<Value, BackendError> {
        let args = serde_json::json!({ "data": text }).to_string();
        self.rex_json(&["block", "call", "-b", target, "write", &args])
    }

    fn agent_read(&self, target: &str, _source: &str, lines: u32) -> Result<Value, BackendError> {
        // `rex capture` has no line limit; take the tail here when asked.
        let text = self.rex(&["capture", "-b", target, "--trim"])?;
        if lines == 0 {
            return Ok(Value::String(text));
        }
        let tail: Vec<&str> = text.lines().collect();
        let start = tail.len().saturating_sub(lines as usize);
        Ok(Value::String(format!("{}\n", tail[start..].join("\n"))))
    }
}

// ---- parsing ----

fn text(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}

fn num(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_u64))
}

fn block_from(v: &Value) -> Option<Block> {
    Some(Block {
        id: text(v, &["block_id", "id"])?,
        window: text(v, &["window_id"]).unwrap_or_default(),
        label: text(v, &["label", "title"]).unwrap_or_default(),
        creator: text(v, &["creator"]),
    })
}

/// `new`/`split --json` return `block_ids`; the first is the created block.
fn first_block_id(v: &Value) -> Option<String> {
    v.get("block_ids")
        .and_then(Value::as_array)
        .and_then(|ids| ids.first())
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn pane_from(b: &Block, session: &str, revision: u64, status: AgentStatus) -> PaneInfo {
    PaneInfo {
        pane_id: b.id.clone(),
        terminal_id: b.id.clone(),
        workspace_id: session.to_string(),
        tab_id: b.window.clone(),
        focused: false,
        agent_status: status,
        revision,
        title: Some(b.label.clone()),
        cwd: None,
        agent: b.creator.clone(),
        agent_session: None,
        display_agent: b.creator.clone(),
        foreground_cwd: None,
        label: Some(b.label.clone()),
        scroll: None,
        state_labels: serde_json::Map::new(),
        terminal_title: Some(b.label.clone()),
        terminal_title_stripped: Some(b.label.clone()),
        tokens: serde_json::Map::new(),
    }
}

/// replace when rex_map lands.
///
/// `program_status` returns the terminal's OSC 7501 records; the first `state`
/// anywhere in the payload is the record this adapter reports. toron-core's
/// `Board` owns the fold, so OSC `idle` is never read as herdr `idle`
/// (finished-and-seen) without this module deciding per `FoldHint`.
fn status_from_program_status(v: &Value) -> AgentStatus {
    let Some(state) = find_state(v) else {
        return AgentStatus::Unknown;
    };
    let mut board = Board::default();
    board.apply(&format!("\u{1b}]7501;state={state}\u{1b}\\"));
    match board.get("").map(|r| r.fold_hint()) {
        Some(toron_core::osc7501::FoldHint::Running) => AgentStatus::Working,
        Some(toron_core::osc7501::FoldHint::NeedsUser) => AgentStatus::Blocked,
        Some(toron_core::osc7501::FoldHint::UnseenDone) => AgentStatus::Done,
        Some(toron_core::osc7501::FoldHint::AtPrompt) => AgentStatus::Idle,
        // Failed/Cleared/absent: unknown never proves completion.
        _ => AgentStatus::Unknown,
    }
}

fn find_state(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => map
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| map.values().find_map(find_state)),
        Value::Array(items) => items.iter().find_map(find_state),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    // PATH is process-global; hold this for the whole fake-rex window so a
    // parallel test never spawns through a half-swapped PATH.
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    /// Put a fake `rex` first on PATH for the lifetime of the guard.
    struct FakeRex {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        old_path: Option<std::ffi::OsString>,
    }

    impl FakeRex {
        fn install(script: &str) -> Self {
            let _lock = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = dir.path().join("rex");
            let mut f = std::fs::File::create(&bin).expect("fake rex");
            f.write_all(script.as_bytes()).expect("write fake rex");
            f.sync_all().expect("sync");
            let mut perms = f.metadata().expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).expect("chmod");
            let old_path = std::env::var_os("PATH");
            let mut path = dir.path().as_os_str().to_os_string();
            if let Some(old) = &old_path {
                path.push(":");
                path.push(old);
            }
            // SAFETY: serialized by PATH_LOCK for the whole guard lifetime.
            unsafe { std::env::set_var("PATH", path) };
            Self {
                _dir: dir,
                _lock,
                old_path,
            }
        }
    }

    impl Drop for FakeRex {
        fn drop(&mut self) {
            unsafe {
                match &self.old_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    /// A session and its one block, in the shapes `rex ls --json` and
    /// `session.list_blocks` return.
    const FAKE_REX: &str = r#"#!/bin/sh
case "$1" in
  ls)
    echo '[{"session_id":"session:fake-1","label":"work","pane_count":1,"tab_count":1}]'
    ;;
  api)
    echo '{"blocks":[{"block_id":"block:fake-1","window_id":"window:fake-1","label":"build"}]}'
    ;;
  block)
    echo '{"records":[{"id":"","state":"working","app":"cargo"}]}'
    ;;
  *)
    echo "fake rex: unsupported $*" >&2
    exit 1
    ;;
esac
"#;

    /// The wiring proof: a session on the fake `rex` becomes a workspace, with
    /// its block counted, and no herdr anywhere in the path.
    #[test]
    fn superlogical_lists_a_session_from_a_fake_rex() {
        let _fake = FakeRex::install(FAKE_REX);
        let b = SuperlogicalBackend;
        let list = b.workspace_list().expect("workspace_list over fake rex");
        assert_eq!(list.len(), 1, "one session, one workspace: {list:#?}");
        let w = &list[0];
        assert_eq!(w.workspace_id, "session:fake-1");
        assert_eq!(w.label, "work");
        assert_eq!(w.pane_count, 1);
        assert_eq!(w.tab_count, 1);
        assert_eq!(b.name(), "superlogical");

        // The same fake's blocks become panes, and its OSC 7501 record is the
        // status the fold reports.
        let panes = b.pane_list(Some("session:fake-1")).expect("pane_list");
        assert_eq!(panes.len(), 1, "one block, one pane: {panes:#?}");
        assert_eq!(panes[0].pane_id, "block:fake-1");
        assert_eq!(panes[0].tab_id, "window:fake-1");
        assert_eq!(panes[0].agent_status, AgentStatus::Working);
    }

    /// A machine without `rex` says so, and says nothing about a release.
    #[test]
    fn superlogical_without_rex_names_the_install_not_a_release() {
        let empty = tempfile::tempdir().expect("tempdir");
        let _lock = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", empty.path()) };
        let res = SuperlogicalBackend.workspace_list();
        unsafe {
            match &old {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        match res {
            Err(e) => {
                assert_eq!(e.code(), &BackendErrorCode::Unsupported);
                assert!(
                    e.message.contains("rex CLI not installed"),
                    "names the fix: {}",
                    e.message
                );
                assert!(
                    !e.message.contains("not yet released"),
                    "never a release excuse: {}",
                    e.message
                );
            }
            Ok(v) => panic!("no rex on PATH must not Ok: {v:#?}"),
        }
    }
}

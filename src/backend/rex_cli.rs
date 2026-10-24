//! Rex CLI edge. The binary and the wait live here. The status fold does not.
//!
//! `REX_BIN_DIR` is read only in [`Rex::resolve`]. A set directory that has
//! no `rex` file is "rex CLI not installed" and does not fall through to
//! PATH, so a test cannot reach the live server by accident.
//!
//! Four outcomes stay apart: success, [`RexFail::Exited`] (rex returned a
//! status), [`RexFail::Interrupted`] (we killed the child at the bound),
//! and a spawn or JSON failure. A hang is not a rex failure.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde_json::Value;
use toron_core::backend::{BackendError, BackendErrorCode, BackendEvent};

use crate::proc::{ProcError, run_bounded};

/// Wall bound around rex's own `--timeout 10s`, so a rex-side timeout can
/// exit before we kill the process group.
pub const REX_WAIT: Duration = Duration::from_secs(12);

/// One resolved rex binary and the wait this call will allow.
pub struct Rex {
    bin: PathBuf,
    wait: Duration,
}

/// What a rex invocation did besides succeed.
#[derive(Debug)]
pub enum RexFail {
    /// The binary is not there. The backend message contains
    /// "rex CLI not installed".
    NotInstalled { detail: String },
    /// The child outlived the bound and was killed. Not a rex exit code.
    Interrupted { waited: Duration },
    /// Rex ran and returned a non-success status.
    Exited { code: Option<i32>, stderr: String },
    /// Stdout was not the JSON the command claims to print.
    BadOutput { message: String },
    /// The process could not be started, and the binary does exist.
    Spawn { message: String },
    /// We refused before spawn (unknown direction, missing block).
    Rejected { message: String },
}

impl RexFail {
    pub fn into_backend(self) -> BackendError {
        match self {
            Self::NotInstalled { detail } => BackendError::new(
                BackendErrorCode::Io,
                format!("rex CLI not installed ({detail})"),
            ),
            Self::Interrupted { waited } => BackendError::timeout(format!(
                "rex wait interrupted after {}",
                bound_label(waited)
            )),
            Self::Exited { code, stderr } => BackendError::new(
                BackendErrorCode::Other("rex_failed".into()),
                format!("rex failed ({code:?}): {stderr}"),
            ),
            Self::BadOutput { message } => BackendError::new(BackendErrorCode::Malformed, message),
            Self::Spawn { message } => BackendError::new(BackendErrorCode::Io, message),
            Self::Rejected { message } => {
                BackendError::new(BackendErrorCode::Other("rejected".into()), message)
            }
        }
    }
}

/// One row of `rex ls --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RexSession {
    pub session_id: String,
    pub label: String,
    pub pane_count: u64,
    pub tab_count: u64,
    pub focused: bool,
    pub active_tab_id: String,
    pub cwd: Option<String>,
}

/// One row of `session.list_blocks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RexBlock {
    pub block_id: String,
    pub label: String,
    pub creator_name: String,
    pub window_id: String,
}

impl Rex {
    pub fn new(bin: PathBuf, wait: Duration) -> Self {
        Self { bin, wait }
    }

    /// `$REX_BIN_DIR/rex` when that variable is set, otherwise `rex` on PATH.
    pub fn resolve() -> Result<Self, RexFail> {
        match std::env::var("REX_BIN_DIR") {
            Ok(dir) if !dir.trim().is_empty() => {
                let path = PathBuf::from(dir.trim()).join("rex");
                if !path.is_file() {
                    return Err(RexFail::NotInstalled {
                        detail: path.display().to_string(),
                    });
                }
                Ok(Self::new(path, REX_WAIT))
            }
            Ok(_) => Err(RexFail::NotInstalled {
                detail: "REX_BIN_DIR is empty".into(),
            }),
            Err(_) => Ok(Self::new(PathBuf::from("rex"), REX_WAIT)),
        }
    }

    pub fn run(&self, args: &[String]) -> Result<Output, RexFail> {
        let mut cmd = std::process::Command::new(&self.bin);
        // Piped stdout is not a terminal, but `--color never` keeps a
        // later redirect from painting the JSON. `--timeout` is rex's own
        // connection bound; the process-group kill below is the hang bound.
        cmd.arg("--color")
            .arg("never")
            .arg("--timeout")
            .arg("10s")
            .args(args);
        match run_bounded(&mut cmd, self.wait) {
            Ok(out) if out.status.success() => Ok(out),
            Ok(out) => Err(RexFail::Exited {
                code: out.status.code(),
                stderr: clip(&String::from_utf8_lossy(&out.stderr)),
            }),
            Err(ProcError::Timeout { .. }) => Err(RexFail::Interrupted { waited: self.wait }),
            Err(ProcError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(RexFail::NotInstalled {
                    detail: self.bin.display().to_string(),
                })
            }
            Err(ProcError::Io(e)) => Err(RexFail::Spawn {
                message: format!("rex spawn: {e}"),
            }),
        }
    }

    pub fn json(&self, args: &[String]) -> Result<Value, RexFail> {
        let out = self.run(args)?;
        parse_stdout_json(&out.stdout)
    }

    pub fn text(&self, args: &[String]) -> Result<String, RexFail> {
        let out = self.run(args)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// A long-lived `rex events` child. Drop kills it. The 12s call bound
    /// does not apply: the stream ends when the caller drops it.
    pub fn spawn(&self, args: &[String]) -> Result<std::process::Child, RexFail> {
        let mut cmd = std::process::Command::new(&self.bin);
        cmd.arg("--color")
            .arg("never")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .args(args);
        cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RexFail::NotInstalled {
                    detail: self.bin.display().to_string(),
                }
            } else {
                RexFail::Spawn {
                    message: format!("rex spawn: {e}"),
                }
            }
        })
    }
}

/// Newline-delimited `rex events --json`. Dropping the stream kills rex.
pub struct RexEventStream {
    child: Option<std::process::Child>,
    rx: std::sync::mpsc::Receiver<std::io::Result<String>>,
    timeout: Option<Duration>,
    panes: Vec<String>,
}

impl RexEventStream {
    pub fn start(mut child: std::process::Child, panes: Vec<String>) -> Result<Self, RexFail> {
        let stdout = child.stdout.take().ok_or_else(|| RexFail::Spawn {
            message: "rex events has no stdout".into(),
        })?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child: Some(child),
            rx,
            timeout: None,
            panes,
        })
    }

    pub fn set_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }

    pub fn next_line(&self) -> Result<Option<String>, RexFail> {
        let received = match self.timeout {
            Some(timeout) => self.rx.recv_timeout(timeout),
            None => self
                .rx
                .recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Ok(line)) => Ok(Some(line)),
            Ok(Err(e)) => Err(RexFail::Spawn {
                message: format!("rex events read: {e}"),
            }),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(RexFail::Interrupted {
                    waited: self.timeout.unwrap_or(Duration::ZERO),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

impl Drop for RexEventStream {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl toron_core::backend::EventStream for RexEventStream {
    fn read_event(&mut self) -> Result<Option<BackendEvent>, BackendError> {
        loop {
            let line = match self.next_line() {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(None),
                Err(err) => return Err(err.into_backend()),
            };
            if line.trim().is_empty() {
                continue;
            }
            let event = rex_map::event_from_line(&line);
            if self.panes.is_empty() || pane_wanted(&event, &self.panes) {
                return Ok(Some(event));
            }
        }
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), BackendError> {
        self.set_timeout(timeout);
        Ok(())
    }
}

fn pane_wanted(event: &BackendEvent, panes: &[String]) -> bool {
    let pane = match event {
        BackendEvent::AgentStatusChanged { pane, .. }
        | BackendEvent::AgentDetected { pane, .. }
        | BackendEvent::PaneExited { pane } => pane.as_str(),
        BackendEvent::EventsLost | BackendEvent::Raw(_) => return true,
    };
    panes.iter().any(|want| want == pane)
}

pub fn ls_args() -> Vec<String> {
    vec!["ls".into(), "--json".into()]
}

pub fn new_args(label: Option<&str>) -> Vec<String> {
    let mut args = vec!["new".into()];
    if let Some(label) = label {
        args.push(label.to_string());
    }
    args.push("--json".into());
    args
}

pub fn list_blocks_args(session: &str) -> Vec<String> {
    vec![
        "api".into(),
        "call".into(),
        "-s".into(),
        session.into(),
        "session.list_blocks".into(),
    ]
}

pub fn program_status_args(session: &str, block: &str) -> Vec<String> {
    vec![
        "block".into(),
        "call".into(),
        "-s".into(),
        session.into(),
        "-b".into(),
        block.into(),
        "program_status".into(),
    ]
}

/// `process` block method: the processes running in the terminal and how
/// its program exited. Same call shape as `program_status`.
pub fn process_args(session: &str, block: &str) -> Vec<String> {
    vec![
        "block".into(),
        "call".into(),
        "-s".into(),
        session.into(),
        "-b".into(),
        block.into(),
        "process".into(),
    ]
}

pub fn write_args(session: &str, block: &str, text: &str) -> Vec<String> {
    vec![
        "block".into(),
        "call".into(),
        "-s".into(),
        session.into(),
        "-b".into(),
        block.into(),
        "write".into(),
        serde_json::json!({ "data": text }).to_string(),
    ]
}

/// `block.close` names one block. It does not name `session.close`.
pub fn block_close_args(session: &str, block: &str) -> Vec<String> {
    vec![
        "api".into(),
        "call".into(),
        "-s".into(),
        session.into(),
        "block.close".into(),
        serde_json::json!({ "block_id": block }).to_string(),
    ]
}

pub fn capture_args(session: &str, block: &str, format: &str) -> Vec<String> {
    vec![
        "capture".into(),
        "-s".into(),
        session.into(),
        "-b".into(),
        block.into(),
        "--format".into(),
        format.into(),
        "--trim".into(),
    ]
}

pub fn split_args(session: &str, block: &str, direction: &str, cwd: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "split".into(),
        "-s".into(),
        session.into(),
        "-b".into(),
        block.into(),
        format!("--split={direction}"),
        "--json".into(),
    ];
    if let Some(cwd) = cwd {
        args.push("-c".into());
        args.push(cwd.to_string());
    }
    args
}

pub fn session_kill_args(session: &str) -> Vec<String> {
    vec!["session".into(), "kill".into(), session.into()]
}

pub fn rename_args(session: &str, block: &str, label: &str) -> Vec<String> {
    // `rename` takes the block positionally: no `-b` flag exists here
    // (unlike `send`/`capture`). Verified against `rex block rename --help`
    // and the live server — the fake accepts anything, so only the live
    // shape guards this.
    vec![
        "block".into(),
        "rename".into(),
        "-s".into(),
        session.into(),
        block.into(),
        label.into(),
    ]
}

pub fn send_key_args(session: &str, block: &str, keys: &[&str]) -> Vec<String> {
    let mut args = vec![
        "block".into(),
        "send-key".into(),
        "-s".into(),
        session.into(),
        "-b".into(),
        block.into(),
    ];
    args.extend(keys.iter().map(|key| (*key).to_string()));
    args
}

pub fn events_args(session: Option<&str>) -> Vec<String> {
    let mut args = vec!["events".into(), "--json".into()];
    if let Some(session) = session {
        args.push("-s".into());
        args.push(session.into());
    }
    args
}

/// `rex do -e <lua> [--args <json>]`: one-shot Lua, result as JSON.
///
/// The script must be handler- and timer-free (see
/// `integrations/rex/session-snapshot.lua`): `rex.on`/`rex.every` keep
/// `rex do` running past the call bound, which the edge reports as
/// `Interrupted`, never as a result. `--json` makes the return value
/// machine-readable; an empty stdout is `BadOutput`, never `Null`.
pub fn do_args(session: Option<&str>, script: &str, args_json: Option<&str>) -> Vec<String> {
    let mut args = vec!["do".into()];
    if let Some(session) = session.filter(|s| !s.is_empty()) {
        args.push("-s".into());
        args.push(session.into());
    }
    args.push("-e".into());
    args.push(script.into());
    if let Some(payload) = args_json.filter(|s| !s.is_empty()) {
        args.push("--args".into());
        args.push(payload.into());
    }
    args.push("--json".into());
    args
}

/// `rex ls --json` is either `{"sessions":[...]}` or a bare array.
pub fn parse_sessions(value: &Value) -> Result<Vec<RexSession>, RexFail> {
    let rows = match value {
        Value::Array(rows) => rows,
        Value::Object(map) => map
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or_else(|| RexFail::BadOutput {
                message: "rex ls JSON missing sessions".into(),
            })?,
        _ => {
            return Err(RexFail::BadOutput {
                message: "rex ls JSON is not a session list".into(),
            });
        }
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(session_row(row)?);
    }
    Ok(out)
}

/// `rex new --json`. Accepts a bare session object or `{"session": ...}`.
pub fn parse_created_session(
    value: &Value,
    label_arg: Option<&str>,
) -> Result<RexSession, RexFail> {
    let row = value.get("session").unwrap_or(value);
    if row.get("session_id").is_none() && row.get("id").is_none() {
        return Err(RexFail::BadOutput {
            message: "rex new JSON missing session_id".into(),
        });
    }
    let mut session = session_row(row)?;
    if session.label == session.session_id {
        if let Some(label) = label_arg.filter(|s| !s.is_empty()) {
            session.label = label.to_string();
        }
    }
    let windows = value
        .get("initial_windows")
        .or_else(|| row.get("initial_windows"))
        .and_then(Value::as_array);
    if let Some(windows) = windows {
        session.tab_count = windows.len() as u64;
        session.pane_count = windows
            .iter()
            .map(|w| {
                w.get("block_ids")
                    .and_then(Value::as_array)
                    .map(|ids| ids.len() as u64)
                    .unwrap_or(0)
            })
            .sum();
        if session.active_tab_id.is_empty() {
            session.active_tab_id = windows
                .first()
                .and_then(|w| w.get("window_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
    }
    Ok(session)
}

pub fn parse_blocks(value: &Value) -> Result<Vec<RexBlock>, RexFail> {
    let rows = value
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or_else(|| RexFail::BadOutput {
            message: "rex list_blocks JSON missing blocks".into(),
        })?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let block_id = field(row, &["block_id", "id"]).ok_or_else(|| RexFail::BadOutput {
            message: "rex block missing block_id".into(),
        })?;
        out.push(RexBlock {
            block_id,
            label: field(row, &["label", "title"]).unwrap_or_default(),
            creator_name: field(row, &["creator_name", "creator"]).unwrap_or_default(),
            window_id: field(row, &["window_id"]).unwrap_or_default(),
        });
    }
    Ok(out)
}

pub fn parse_created_block_id(value: &Value) -> Result<String, RexFail> {
    if let Some(id) = field(value, &["block_id"]) {
        return Ok(id);
    }
    if let Some(block) = value.get("block") {
        if let Some(id) = field(block, &["block_id", "id"]) {
            return Ok(id);
        }
    }
    if let Some(id) = value
        .get("block_ids")
        .and_then(Value::as_array)
        .and_then(|ids| ids.first())
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return Ok(id.to_string());
    }
    Err(RexFail::BadOutput {
        message: "rex split JSON missing block_id".into(),
    })
}

fn session_row(row: &Value) -> Result<RexSession, RexFail> {
    let session_id = field(row, &["session_id", "id"]).ok_or_else(|| RexFail::BadOutput {
        message: "rex session missing session_id".into(),
    })?;
    let label = field(row, &["label"]).unwrap_or_else(|| session_id.clone());
    Ok(RexSession {
        session_id,
        label,
        pane_count: row.get("pane_count").and_then(Value::as_u64).unwrap_or(0),
        tab_count: row
            .get("tab_count")
            .or_else(|| row.get("window_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        focused: row.get("focused").and_then(Value::as_bool).unwrap_or(false),
        active_tab_id: row
            .get("active_window_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        cwd: row.get("cwd").and_then(Value::as_str).map(str::to_string),
    })
}

fn field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

fn parse_stdout_json(bytes: &[u8]) -> Result<Value, RexFail> {
    let text = String::from_utf8(bytes.to_vec()).map_err(|e| RexFail::BadOutput {
        message: format!("rex stdout is not utf-8: {e}"),
    })?;
    let text = text.trim();
    if text.is_empty() {
        return Err(RexFail::BadOutput {
            message: "rex returned empty stdout".into(),
        });
    }
    serde_json::from_str(text).map_err(|e| RexFail::BadOutput {
        message: format!("rex JSON: {e}"),
    })
}

fn clip(text: &str) -> String {
    let mut clipped: String = text.chars().take(400).collect();
    if text.chars().count() > 400 {
        clipped.push_str("…");
    }
    clipped
}

fn bound_label(waited: Duration) -> String {
    if waited.as_secs() >= 1 && waited.subsec_nanos() == 0 {
        format!("{}s", waited.as_secs())
    } else {
        format!("{}ms", waited.as_millis())
    }
}

/// Pure status fold. It does not read the environment or spawn rex.
///
/// This is a module inside `rex_cli.rs` because `src/backend/rex_map.rs` and
/// `src/backend/mod.rs` are exclusively leased by PurpleGorge for bd-i328.
pub mod rex_map {
    use serde_json::Value;
    use toron_core::backend::{AgentStatus, BackendEvent, PaneId};
    use toron_core::osc7501::{Board, ProgramState, parse_report};

    use crate::backend::rex_map::fold_status;

    /// Map one `program_status` payload to the trait status.
    ///
    /// OSC `idle` is at a prompt. OSC `error` is Unknown, never Done.
    /// The root record wins. With no root, any blocked child is Blocked.
    ///
    /// ponytail: no-root, non-blocked children fold newest-first.
    pub fn status_from_program_status(value: &Value) -> AgentStatus {
        let (board, order) = board_of(value);
        fold_status(&board, &order)
    }

    /// The board behind one payload, shared by the records fold and the
    /// process fallback so both read one parse.
    pub fn board_of(value: &Value) -> (Board, Vec<String>) {
        let mut board = Board::default();
        let mut order: Vec<String> = Vec::new();
        for record in record_slice(value) {
            apply_record(&mut board, &mut order, record);
        }
        (board, order)
    }

    /// What the terminal's foreground process looks like. Empty when the
    /// shape is missing or odd — the fold reads that as Unknown.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct ProcessView {
        pub foreground: String,
        pub cwd: Option<String>,
    }

    /// Shells: a foreground shell is at a prompt (Idle). Anything else that
    /// is not a known agent is Unknown, never inferred.
    const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "fish", "dash"];

    /// Parse one `process` block-method payload into the foreground view.
    pub fn parse_process_view(value: &Value) -> ProcessView {
        let foreground = value.get("foreground");
        ProcessView {
            foreground: foreground
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            cwd: foreground
                .and_then(|f| f.get("cwd"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    /// Liveness without cooperation: a known agent binary in front is
    /// Working, a shell is at a prompt. Explicit records always win over
    /// this — callers only reach it on an empty board.
    pub fn process_status(view: &ProcessView) -> AgentStatus {
        match crate::pane_write::Harness::from_kind(&view.foreground) {
            crate::pane_write::Harness::Other => {
                if SHELLS.contains(&view.foreground.as_str()) {
                    AgentStatus::Idle
                } else {
                    AgentStatus::Unknown
                }
            }
            _ => AgentStatus::Working,
        }
    }

    /// Records first, process fallback: an empty board (no program ever
    /// reported — the opencode steady state) folds the foreground process;
    /// any record at all keeps the records fold, including explicit Unknown.
    pub fn status_for_block(board: &Board, order: &[String], view: &ProcessView) -> AgentStatus {
        if board.is_empty() {
            process_status(view)
        } else {
            fold_status(board, order)
        }
    }

    /// Rex `--split` directions, plus the herdr words the trait already passes.
    pub fn split_direction(raw: &str) -> Option<&'static str> {
        Some(match raw {
            "left" => "left",
            "right" | "horizontal" => "right",
            "above" | "up" => "above",
            "below" | "down" | "vertical" => "below",
            _ => return None,
        })
    }

    /// `rex capture` has no line cap. `0` means the whole screen.
    pub fn tail_lines(text: &str, lines: u32) -> String {
        if lines == 0 {
            return text.to_string();
        }
        let all: Vec<&str> = text.lines().collect();
        let start = all.len().saturating_sub(lines as usize);
        let mut out = all[start..].join("\n");
        if text.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// `rex capture --format`. Unknown sources stay text. No tmux flags.
    pub fn capture_format(source: &str) -> &'static str {
        match source {
            "html" => "html",
            "vt" => "vt",
            _ => "text",
        }
    }

    fn record_slice(value: &Value) -> Vec<&Value> {
        if let Some(rows) = value.get("records").and_then(Value::as_array) {
            return rows.iter().collect();
        }
        match value {
            Value::Array(rows) => rows.iter().collect(),
            Value::String(_) => vec![value],
            Value::Object(map) if map.contains_key("state") || map.contains_key("osc") => {
                vec![value]
            }
            _ => Vec::new(),
        }
    }

    /// Apply one `program_status` record to the board. Raw strings go to
    /// the parser as-is; objects naming a raw report key (`osc`, `raw`,
    /// `report`, `sequence`) apply the first report the parser accepts;
    /// otherwise a `state` word renders through the same grammar, so an id
    /// outside the OSC id grammar drops the record. Anything `parse_report`
    /// refuses is not a record.
    fn apply_record(board: &mut Board, order: &mut Vec<String>, value: &Value) {
        match value {
            Value::String(raw) => {
                if let Some(report) = parse_report(raw) {
                    order.push(report.id.clone().unwrap_or_default());
                    board.apply(raw);
                }
            }
            Value::Object(map) => {
                for key in ["osc", "raw", "report", "sequence"] {
                    if let Some(Value::String(raw)) = map.get(key) {
                        if let Some(report) = parse_report(raw) {
                            order.push(report.id.clone().unwrap_or_default());
                            board.apply(raw);
                            return;
                        }
                    }
                }
                let Some(state) = map.get("state").and_then(Value::as_str) else {
                    return;
                };
                if word_state(state).is_none() {
                    return;
                }
                let id = map.get("id").and_then(Value::as_str).unwrap_or("");
                let raw = if id.is_empty() {
                    format!("\u{1b}]7501;state={state}\u{1b}\\")
                } else {
                    format!("\u{1b}]7501;state={state}:id={id}\u{1b}\\")
                };
                if let Some(report) = parse_report(&raw) {
                    order.push(report.id.clone().unwrap_or_default());
                    board.apply(&raw);
                }
            }
            _ => {}
        }
    }

    fn word_state(raw: &str) -> Option<ProgramState> {
        Some(match raw {
            "idle" => ProgramState::Idle,
            "working" => ProgramState::Working,
            "done" => ProgramState::Done,
            "blocked" => ProgramState::Blocked,
            "error" => ProgramState::Error,
            "clear" => ProgramState::Clear,
            _ => return None,
        })
    }

    /// One `rex events --json` line. Unknown shapes stay `Raw`.
    pub fn event_from_line(line: &str) -> BackendEvent {
        let line = line.trim();
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return BackendEvent::Raw(line.to_string());
        };
        let inner = value.pointer("/payload/event").unwrap_or(&value);
        let kind = inner.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = inner.get("payload").cloned().unwrap_or(Value::Null);
        let pane = payload
            .get("block_id")
            .or_else(|| payload.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if kind.ends_with("program_status_changed") {
            let record = payload.get("record").cloned().unwrap_or(Value::Null);
            let status = status_from_program_status(&json_records(&record));
            return BackendEvent::AgentStatusChanged {
                pane: PaneId::new(pane),
                agent: payload
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status,
            };
        }
        if kind.ends_with("block_closed") {
            return BackendEvent::PaneExited {
                pane: PaneId::new(pane),
            };
        }
        BackendEvent::Raw(line.to_string())
    }

    fn json_records(record: &Value) -> Value {
        if record.get("records").is_some() {
            return record.clone();
        }
        serde_json::json!({ "records": [record] })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        fn osc(body: &str) -> String {
            format!("\u{1b}]7501;{body}\u{1b}\\")
        }

        /// Root OSC `error` is Unknown, including when a child says done.
        /// A separate done OSC must still be Done, so skipping the parser
        /// cannot pass this test.
        #[test]
        fn osc_error_is_not_done() {
            let status = status_from_program_status(&json!({
                "records": [
                    osc("state=error:app=freebuff"),
                    {"id": "child", "state": "done"}
                ]
            }));
            assert_eq!(status, AgentStatus::Unknown, "root OSC error");
            assert_ne!(status, AgentStatus::Done);
            assert_ne!(status, AgentStatus::Idle);

            let finished = status_from_program_status(&json!({
                "records": [osc("state=done:app=freebuff")]
            }));
            assert_eq!(
                finished,
                AgentStatus::Done,
                "a done OSC must classify, so an unparsed record cannot pass as Unknown"
            );

            let word = status_from_program_status(&json!({
                "records": [{"state": "error"}]
            }));
            assert_eq!(word, AgentStatus::Unknown, "JSON error is not done");
            assert_ne!(word, AgentStatus::Idle);
        }

        #[test]
        fn root_wins_and_a_blocked_child_without_a_root_is_blocked() {
            assert_eq!(
                status_from_program_status(&json!({"records": []})),
                AgentStatus::Unknown
            );
            let blocked = status_from_program_status(&json!({
                "records": [
                    {"id": "eu-west", "state": "working"},
                    {"id": "us-east", "state": "blocked"}
                ]
            }));
            assert_eq!(blocked, AgentStatus::Blocked);
            let root_working = status_from_program_status(&json!({
                "records": [
                    {"state": "working"},
                    {"id": "eu-west", "state": "blocked"}
                ]
            }));
            assert_eq!(root_working, AgentStatus::Working);
            let idle = status_from_program_status(&json!({
                "records": [osc("state=idle:app=claude-code")]
            }));
            assert_eq!(idle, AgentStatus::Idle, "OSC idle is at a prompt");
        }

        /// Both folds read one policy: the live `program_status` entry and
        /// the projector agree on every shared fixture, so herdr and rex
        /// panes classify identically.
        #[test]
        fn both_folds_agree_on_shared_fixtures() {
            use crate::backend::rex_map::status_from_records;
            for records in [
                r#"[]"#,
                r#"[{"state":"error","app":"freebuff"}]"#,
                r#"[{"state":"idle"}]"#,
                r#"[{"state":"blocked"}]"#,
                r#"[{"state":"working"},{"id":"eu-west","state":"blocked"}]"#,
                r#"[{"id":"us-east","state":"working"},{"id":"eu-west","state":"blocked"}]"#,
                r#"[{"id":"us-east","state":"working"}]"#,
                r#"[{"id":"eu-west","state":"blocked"},{"state":"clear","id":"eu-west"}]"#,
            ] {
                let value: Value = serde_json::from_str(records).expect("fixture is JSON");
                let live = status_from_program_status(&json!({ "records": value }));
                assert_eq!(status_from_records(records), live, "{records}");
            }
        }

        fn process_view(name: &str) -> ProcessView {
            ProcessView {
                foreground: name.to_string(),
                cwd: None,
            }
        }

        /// The process fallback reads the foreground program: a known agent
        /// is Working, a shell is at a prompt, anything else is Unknown.
        #[test]
        fn process_view_folds_foreground_program() {
            assert_eq!(
                process_status(&process_view("opencode")),
                AgentStatus::Working
            );
            assert_eq!(process_status(&process_view("pi")), AgentStatus::Working);
            assert_eq!(process_status(&process_view("zsh")), AgentStatus::Idle);
            assert_eq!(process_status(&process_view("bash")), AgentStatus::Idle);
            assert_eq!(process_status(&process_view("")), AgentStatus::Unknown);
            assert_eq!(
                process_status(&process_view("postgres")),
                AgentStatus::Unknown
            );
            let parsed = parse_process_view(&json!({
                "foreground": {"name": "opencode", "cwd": "/tmp"},
                "last_exit": null,
            }));
            assert_eq!(parsed.foreground, "opencode");
            assert_eq!(parsed.cwd.as_deref(), Some("/tmp"));
            assert_eq!(parse_process_view(&json!({})), ProcessView::default());
            assert_eq!(
                parse_process_view(&json!({"foreground": null})),
                ProcessView::default()
            );
        }

        /// Records win over the process view: an explicit error stays
        /// Unknown even with an agent in front; an empty board folds the
        /// foreground instead.
        #[test]
        fn records_win_over_the_process_view() {
            let (board, order) = board_of(&json!({
                "records": [{"state": "error", "app": "freebuff"}]
            }));
            assert_eq!(
                status_for_block(&board, &order, &process_view("opencode")),
                AgentStatus::Unknown
            );
            let (empty, empty_order) = board_of(&json!({ "records": [] }));
            assert_eq!(
                status_for_block(&empty, &empty_order, &process_view("opencode")),
                AgentStatus::Working
            );
            assert_eq!(
                status_for_block(&empty, &empty_order, &process_view("zsh")),
                AgentStatus::Idle
            );
            assert_eq!(
                status_for_block(&empty, &empty_order, &process_view("")),
                AgentStatus::Unknown
            );
        }
    }
}

/// Holds `REX_BIN_DIR` for one fake binary. Never prepends PATH, so the
/// real rex stays off the lookup.
///
/// One lock domain for the process-global var: the guard holds `REX_LOCK`
/// and then the crate-wide env lock (`lock_env`, which also pins a hermetic
/// `XDG_STATE_HOME`), in that order everywhere. The factory feature-off
/// test mutates the same var under `lock_env` alone, so either side waits
/// for the other — no interleaved empty-dir reads, no deadlock (nothing
/// takes the locks in the reverse order).
#[cfg(test)]
pub(crate) struct RexBinGuard {
    _dir: tempfile::TempDir,
    log: PathBuf,
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
    _env: crate::config::TestStateHome,
}

#[cfg(test)]
static REX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
impl RexBinGuard {
    pub(crate) fn install(script: &str) -> Self {
        let _lock = REX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::config::lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("log");
        let script = script.replace("@@LOG@@", &shell_quote(&log));
        let bin = dir.path().join("rex");
        write_exec(&bin, &script);
        let prev = std::env::var_os("REX_BIN_DIR");
        unsafe { std::env::set_var("REX_BIN_DIR", dir.path()) };
        Self {
            _dir: dir,
            log,
            prev,
            _lock,
            _env,
        }
    }

    /// A directory with no `rex` file. Resolve must not search PATH.
    pub(crate) fn empty() -> Self {
        let _lock = REX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = crate::config::lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("REX_BIN_DIR");
        unsafe { std::env::set_var("REX_BIN_DIR", dir.path()) };
        Self {
            _dir: dir,
            log: PathBuf::new(),
            prev,
            _lock,
            _env,
        }
    }

    pub(crate) fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[cfg(test)]
impl Drop for RexBinGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev {
                Some(value) => std::env::set_var("REX_BIN_DIR", value),
                None => std::env::remove_var("REX_BIN_DIR"),
            }
        }
    }
}

#[cfg(test)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

#[cfg(test)]
pub(crate) fn write_exec(path: &Path, script: &str) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::File::create(path).expect("create rex");
    file.write_all(script.as_bytes()).expect("write rex");
    let mut perms = file.metadata().expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const LS_FAKE: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> @@LOG@@
for arg in "$@"; do
  case "$arg" in
    *herdr*) echo "refusing herdr" >&2; exit 3 ;;
  esac
done
for arg in "$@"; do
  if [ "$arg" = "ls" ]; then
    printf '%s\n' '{"sessions":[{"session_id":"sess-fake","label":"demo"}]}'
    exit 0
  fi
done
echo "unexpected $*" >&2
exit 2
"#;

    #[test]
    fn a_fake_rex_lists_sessions() {
        let guard = RexBinGuard::install(LS_FAKE);
        let rex = Rex::resolve().expect("REX_BIN_DIR/rex");
        let value = rex.json(&ls_args()).expect("ls --json");
        let sessions = parse_sessions(&value).expect("sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-fake");
        assert_eq!(sessions[0].label, "demo");
        let log = guard.log_text();
        assert!(log.contains("ls"), "{log}");
        assert!(log.contains("--json"), "{log}");
        assert!(!log.contains("herdr"), "{log}");
    }

    #[test]
    fn block_close_does_not_close_the_session() {
        let joined = block_close_args("sess-fake", "block-fake").join(" ");
        assert!(joined.contains("block.close"), "{joined}");
        assert!(joined.contains("block-fake"), "{joined}");
        assert!(!joined.contains("session.close"), "{joined}");
        assert!(!joined.contains("session.destroy"), "{joined}");
        assert!(!joined.contains("kill"), "{joined}");
    }

    const DO_FAKE: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> @@LOG@@
for arg in "$@"; do
  case "$arg" in
    *herdr*) echo "refusing herdr" >&2; exit 3 ;;
  esac
done
for arg in "$@"; do
  if [ "$arg" = "do" ]; then
    printf '%s\n' '{"label":"work","windows":1}'
    exit 0
  fi
done
echo "unexpected $*" >&2
exit 2
"#;

    /// `rex do -e` runs one-shot Lua and answers JSON. A script with
    /// handlers would outlive the call bound and surface as Interrupted,
    /// so only handler-free scripts (like `session-snapshot.lua`) ride
    /// this path.
    #[test]
    fn rex_do_runs_one_shot_lua_as_json() {
        let guard = RexBinGuard::install(DO_FAKE);
        let rex = Rex::resolve().expect("REX_BIN_DIR/rex");
        let value = rex
            .json(&do_args(
                Some("session:fake-1"),
                "return { ok = true }",
                Some(r#"{"limit":2}"#),
            ))
            .expect("do --json");
        assert_eq!(value.get("label").and_then(Value::as_str), Some("work"));
        let bare = rex
            .json(&do_args(None, "return { ok = true }", None))
            .expect("do without session or args");
        assert_eq!(bare.get("windows").and_then(Value::as_u64), Some(1));
        let log = guard.log_text();
        for flag in ["do", "-e", "--args", "--json", "-s"] {
            assert!(log.contains(flag), "{log}");
        }
        assert!(!log.contains("herdr"), "{log}");
    }

    /// A child that ignores `--timeout` is killed. That is an interruption,
    /// not `rex_failed`.
    #[test]
    fn a_hung_rex_is_an_interruption_not_a_rex_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("rex");
        write_exec(&bin, "#!/bin/sh\nsleep 3\n");
        let rex = Rex::new(bin, Duration::from_millis(300));
        let started = Instant::now();
        let err = rex.run(&ls_args()).expect_err("the bound must win");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the bound must fire before sleep 3: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(err, RexFail::Interrupted { .. }),
            "interruption, not a rex exit: {err:?}"
        );
        let message = err.into_backend().to_string();
        assert!(message.contains("interrupted"), "{message}");
        assert!(!message.contains("rex_failed"), "{message}");
    }
}

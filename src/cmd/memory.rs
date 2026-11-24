//! `flywheel memory` — Chie Memory trap preflight and lesson recording.
//!
//! Surfaces known traps/invariants at briefing/claim time and records lessons
//! back into the memory plane. Only a truly unreachable chie degrades clean;
//! a usage error is loud, because a check that cannot fail is not a check.

use anyhow::{Result, bail};
use clap::Subcommand;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const CHIE_SUBPROCESS_TIMEOUT: Duration = Duration::from_millis(1500);

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

fn run_chie_bounded(cmd: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("missing stdout pipe"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("missing stderr pipe"))?;
    let mut guard = ChildGuard(Some(child));

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = done_tx.send(());
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    if done_rx.recv_timeout(timeout).is_err() {
        guard.kill_and_wait();
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("chie timed out after {} ms", timeout.as_millis()),
        ));
    }

    let status = guard
        .0
        .take()
        .ok_or_else(|| std::io::Error::other("missing child handle"))?
        .wait()?;
    let stdout_bytes = out_handle.join().unwrap_or_default();
    let stderr_bytes = err_handle.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

use crate::config::Config;
use crate::output::print_json;

#[derive(Subcommand, Debug, Clone)]
pub enum MemoryCmd {
    /// Check for known traps/conclusions matching a query (path, bead, or text).
    /// Offline/unreachable chie => clean (no traps), exit 0.
    CheckTrap {
        /// Query to match (bead id, file/path, or free text).
        query: String,
        /// Project slug (scopes the check when possible).
        #[arg(long)]
        project: Option<String>,
        /// Actor pin/name (accepted for parity; forwarded where supported).
        #[arg(long = "as", name = "as")]
        r#as: Option<String>,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Record a lesson/conclusion in Chie Memory.
    RecordLesson {
        /// Lesson content.
        content: String,
        /// Memory kind (e.g. decision, preference_signal, error). Defaults to
        /// chie_remember semantics when omitted.
        #[arg(long)]
        kind: Option<String>,
        /// Project slug (scopes the record when possible).
        #[arg(long)]
        project: Option<String>,
        /// Actor pin/name (accepted for parity; forwarded where supported).
        #[arg(long = "as", name = "as")]
        r#as: Option<String>,
        /// Quarantine as a candidate conclusion instead of active immediately.
        #[arg(long)]
        candidate: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

pub fn cmd_memory(cfg: &Config, cmd: &MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::CheckTrap {
            query,
            project,
            r#as,
            json,
        } => cmd_check_trap(cfg, query, project.as_deref(), r#as.as_deref(), *json),
        MemoryCmd::RecordLesson {
            content,
            kind,
            project,
            r#as,
            candidate,
            json,
        } => cmd_record_lesson(
            cfg,
            content,
            kind.as_deref(),
            project.as_deref(),
            r#as.as_deref(),
            *candidate,
            *json,
        ),
    }
}

fn cmd_check_trap(
    cfg: &Config,
    query: &str,
    project: Option<&str>,
    _as_agent: Option<&str>,
    json: bool,
) -> Result<()> {
    let project_dir = project.map(|p| crate::project_dir(cfg, p));
    let check = check_trap(query, project_dir.as_deref());
    if check.status == TrapStatus::Failed {
        bail!(
            "memory check-trap failed: {}",
            check.detail.as_deref().unwrap_or("unknown chie error")
        );
    }
    if json {
        print_json(&serde_json::json!({
            "ok": true,
            "status": check.status,
            "detail": check.detail,
            "query": query,
            "traps": check.traps,
            "count": check.traps.len(),
        }));
        return Ok(());
    }
    for t in &check.traps {
        println!("- {t}");
    }
    match check.status {
        TrapStatus::Ok if check.traps.is_empty() => println!("no traps"),
        TrapStatus::Ok => {}
        TrapStatus::Offline => println!("no traps found (chie offline)"),
        TrapStatus::Timeout | TrapStatus::Failed => println!(
            "memory check incomplete: {}",
            check.detail.as_deref().unwrap_or("chie timed out")
        ),
    }
    Ok(())
}

fn cmd_record_lesson(
    cfg: &Config,
    content: &str,
    kind: Option<&str>,
    project: Option<&str>,
    as_agent: Option<&str>,
    candidate: bool,
    json: bool,
) -> Result<()> {
    let project_dir = project.map(|p| crate::project_dir(cfg, p));
    let recorded =
        record_lesson_with_options(content, kind, project_dir.as_deref(), as_agent, candidate);
    if json {
        print_json(&serde_json::json!({
            "ok": recorded,
            "recorded": recorded,
            "offline": !recorded,
        }));
    } else if recorded {
        println!("recorded");
    } else {
        eprintln!("flywheel: memory record unavailable (chie offline?) — lesson not recorded");
    }
    // Graceful: offline/unreachable must not fail the caller's flow.
    Ok(())
}

const MAX_TRAPS: usize = 20;
const MAX_BRIEF_TRAPS: usize = 5;
const MAX_QUERY_CHARS: usize = 600;
const MAX_PATH_CHECKS: usize = 12;
const CHIE_HTTP_TIMEOUT: Duration = Duration::from_millis(250);
const DEFAULT_CHIE_HTTP_URL: &str = "http://127.0.0.1:18081";

/// How complete a trap check is. Ordered by severity: a fold keeps the worst
/// call's status, so one timed-out call marks the whole result partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrapStatus {
    Ok,
    Offline,
    Timeout,
    Failed,
}

/// Traps found plus how far the check got. Callers render the status so a
/// partial or failed check never reads as "no traps".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrapCheck {
    pub traps: Vec<String>,
    pub status: TrapStatus,
    pub detail: Option<String>,
}

#[derive(Debug)]
enum CallOutcome {
    Found(Vec<String>),
    Offline,
    Timeout(String),
    Failed(String),
}

#[derive(Debug, Default)]
struct TrapCall {
    query: Option<String>,
    command: Option<String>,
    path: Option<String>,
}

impl TrapCall {
    fn argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        for (flag, value) in [
            ("--query", &self.query),
            ("--command", &self.command),
            ("--path", &self.path),
        ] {
            if let Some(v) = value {
                argv.push(flag.to_string());
                argv.push(v.clone());
            }
        }
        argv.push("--json".to_string());
        argv
    }
}

fn path_like(q: &str) -> bool {
    q.contains('/') || q.ends_with(".rs") || q.ends_with(".toml") || q.ends_with(".md")
}

fn chie_http_base_url() -> Option<String> {
    if let Ok(raw) = std::env::var("FLYWHEEL_CHIE_HTTP_URL") {
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("off") {
            return None;
        }
        return Some(trimmed.to_string());
    }
    if cfg!(test) {
        return None;
    }
    Some(DEFAULT_CHIE_HTTP_URL.to_string())
}

#[derive(Debug)]
enum HttpTrapResult {
    Hit(TrapCheck),
    Fallback,
}

fn try_briefing_traps_http(
    base_url: &str,
    query: Option<&str>,
    paths: &[String],
    project_dir: Option<&Path>,
) -> HttpTrapResult {
    let url = format!("{base_url}/v1/check-trap");
    let project = chie_project(project_dir);
    let body = serde_json::json!({
        "query": query,
        "paths": paths,
        "project": project,
    });
    let agent = ureq::AgentBuilder::new().timeout(CHIE_HTTP_TIMEOUT).build();
    let resp = match agent.post(&url).send_json(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(404 | 405 | 501 | 503, _)) => return HttpTrapResult::Fallback,
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            return HttpTrapResult::Hit(TrapCheck {
                traps: Vec::new(),
                status: TrapStatus::Failed,
                detail: Some(format!("HTTP {code}: {text}")),
            });
        }
        Err(ureq::Error::Transport(t)) => {
            let msg = t.to_string();
            let low = msg.to_ascii_lowercase();
            if low.contains("timed out") || low.contains("timeout") {
                return HttpTrapResult::Hit(TrapCheck {
                    traps: Vec::new(),
                    status: TrapStatus::Timeout,
                    detail: Some(msg),
                });
            }
            return HttpTrapResult::Fallback;
        }
    };
    let doc: serde_json::Value = match resp.into_json() {
        Ok(d) => d,
        Err(e) => {
            return HttpTrapResult::Hit(TrapCheck {
                traps: Vec::new(),
                status: TrapStatus::Failed,
                detail: Some(format!("invalid JSON from chie HTTP: {e}")),
            });
        }
    };
    let mut check = fold_outcomes(vec![CallOutcome::Found(extract_trap_lines_json(&doc))]);
    check.traps.truncate(MAX_BRIEF_TRAPS);
    HttpTrapResult::Hit(check)
}

pub(crate) fn briefing_traps_with_http_url(
    http_base_url: Option<&str>,
    objective: &str,
    bead: Option<&str>,
    project_dir: Option<&Path>,
) -> TrapCheck {
    let paths: Vec<String> = project_dir
        .map(modified_paths)
        .unwrap_or_default()
        .into_iter()
        .take(MAX_PATH_CHECKS)
        .collect();
    let mut terms: Vec<&str> = vec![objective.trim()];
    terms.extend(bead.map(str::trim));
    terms.extend(paths.iter().map(String::as_str));
    let query: String = terms
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect();
    if let Some(base_url) = http_base_url {
        let q = (!query.is_empty()).then_some(query.as_str());
        if let HttpTrapResult::Hit(check) =
            try_briefing_traps_http(base_url, q, &paths, project_dir)
        {
            return check;
        }
    }
    let mut calls = Vec::new();
    if !query.is_empty() {
        calls.push(TrapCall {
            query: Some(query),
            ..TrapCall::default()
        });
    }
    calls.extend(paths.into_iter().map(|p| TrapCall {
        path: Some(p),
        ..TrapCall::default()
    }));
    let mut check = run_trap_calls(&calls, project_dir);
    check.traps.truncate(MAX_BRIEF_TRAPS);
    check
}

/// Briefing preflight: the objective, bead and modified paths go to chie as
/// ONE HTTP call when `:18081` answers, falling back to bounded subprocesses.
pub fn briefing_traps(
    objective: &str,
    bead: Option<&str>,
    project_dir: Option<&Path>,
) -> TrapCheck {
    let base = chie_http_base_url();
    briefing_traps_with_http_url(base.as_deref(), objective, bead, project_dir)
}

/// `flywheel memory check-trap`: one chie call that reads the query as text,
/// as a command, and (when it looks like one) as a path.
pub fn check_trap(query: &str, project_dir: Option<&Path>) -> TrapCheck {
    let q = query.trim();
    if q.is_empty() {
        return fold_outcomes(Vec::new());
    }
    let call = TrapCall {
        query: Some(q.to_string()),
        command: Some(q.to_string()),
        path: path_like(q).then(|| q.to_string()),
    };
    run_trap_calls(&[call], project_dir)
}

fn run_trap_calls(calls: &[TrapCall], project_dir: Option<&Path>) -> TrapCheck {
    let outcomes = std::thread::scope(|s| {
        let handles: Vec<_> = calls
            .iter()
            .map(|call| s.spawn(move || run_check_trap(&call.argv(), project_dir)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| CallOutcome::Failed("check-trap worker panicked".into()))
            })
            .collect()
    });
    fold_outcomes(outcomes)
}

fn fold_outcomes(outcomes: Vec<CallOutcome>) -> TrapCheck {
    let mut seen = BTreeSet::new();
    let mut traps = Vec::new();
    let mut status = TrapStatus::Ok;
    let mut details = Vec::new();
    for outcome in outcomes {
        match outcome {
            CallOutcome::Found(lines) => {
                for line in lines {
                    let line = normalize_line(&line);
                    if !line.is_empty() && insert_lesson_once(&mut seen, &line) {
                        traps.push(line);
                    }
                }
            }
            CallOutcome::Offline => status = status.max(TrapStatus::Offline),
            CallOutcome::Timeout(d) => {
                status = status.max(TrapStatus::Timeout);
                details.push(d);
            }
            CallOutcome::Failed(d) => {
                status = status.max(TrapStatus::Failed);
                details.push(d);
            }
        }
    }
    traps.truncate(MAX_TRAPS);
    details.dedup();
    TrapCheck {
        traps,
        status,
        detail: (!details.is_empty()).then(|| details.join("; ")),
    }
}

fn normalize_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip the framing chie wraps around a stored lesson. Both the old and the
/// new chiebukuro renderings add `Rule:` repeatedly; a bare lesson is what we
/// want to send and to compare (bd-7szny).
fn strip_lesson_framing(s: &str) -> &str {
    let mut core = s.trim();
    loop {
        let before = core;
        for prefix in ["Rule:", "Trap:", "Proof:"] {
            if let Some(rest) = core.strip_prefix(prefix) {
                core = rest.trim_start();
                break;
            }
        }
        if core.len() == before.len() {
            return core;
        }
    }
}

/// Where chie appends its trap matcher. Cutting at the FIRST backtick would
/// merge two distinct lessons whose rule text merely mentions code (`a` vs `b`),
/// so cut only at the framing chie itself adds (bd-7szny follow-up).
fn trap_boundary(bare: &str) -> usize {
    let mut end = bare.len();
    for marker in [" Trap: `", ": `"] {
        if let Some(at) = bare.find(marker) {
            end = end.min(at);
        }
    }
    end
}

/// The stable identity of a lesson across chie renderings: framing removed,
/// text after chie's trap matcher dropped, whitespace normalized.
fn lesson_core(s: &str) -> String {
    let bare = strip_lesson_framing(s);
    let before_trap = &bare[..trap_boundary(bare)];
    let mut core = before_trap.trim();
    for suffix in ["Trap:", "Proof:", "Rule:"] {
        if let Some(rest) = core.strip_suffix(suffix) {
            core = rest.trim_end();
        }
    }
    core.trim_end_matches(['.', ';', ',', ':'])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The ONE lesson-dedup insertion key, shared by chie recall and the final
/// preflight edge so a mutant that kills dedup turns both layers off (bd-7szny).
fn insert_lesson_once(seen: &mut BTreeSet<String>, raw: &str) -> bool {
    seen.insert(lesson_core(raw))
}

fn chie_bin() -> String {
    std::env::var("FLYWHEEL_CHIE_BIN").unwrap_or_else(|_| "chie".to_string())
}

/// A slug is resolved to a directory by flywheel, but chie interprets a
/// relative `--project` from its own cwd. Canonicalize so both doors agree
/// (bd-97uwg).
fn chie_project(dir: Option<&Path>) -> Option<String> {
    dir.map(|d| {
        let abs = std::fs::canonicalize(d).unwrap_or_else(|_| d.to_path_buf());
        abs.to_string_lossy().into_owned()
    })
}

fn chie_unreachable(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    low.contains("unreachable")
        || low.contains("refused")
        || low.contains("connect")
        || low.contains("daemon")
}

fn run_check_trap(argv: &[String], project_dir: Option<&Path>) -> CallOutcome {
    let mut cmd = Command::new(chie_bin());
    cmd.args(["memory", "check-trap"]);
    if let Some(dir) = chie_project(project_dir) {
        cmd.args(["--project", &dir]);
    }
    cmd.args(argv);
    let out = match run_chie_bounded(&mut cmd, CHIE_SUBPROCESS_TIMEOUT) {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CallOutcome::Offline,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            return CallOutcome::Timeout(e.to_string());
        }
        Err(e) => return CallOutcome::Failed(format!("cannot run chie check-trap: {e}")),
    };
    if out.status.success() {
        return match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            Ok(doc) => CallOutcome::Found(extract_trap_lines_json(&doc)),
            Err(e) => CallOutcome::Failed(format!("chie check-trap returned invalid JSON: {e}")),
        };
    }
    let err_text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if chie_unreachable(&err_text) {
        return CallOutcome::Offline;
    }
    let detail = if err_text.is_empty() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        err_text
    };
    CallOutcome::Failed(format!(
        "chie memory check-trap failed ({}): {detail}",
        argv.join(" ")
    ))
}

fn extract_trap_lines_json(v: &serde_json::Value) -> Vec<String> {
    if let Some(arr) = v.get("traps").and_then(|x| x.as_array()) {
        return strings_from_array(arr);
    }
    if let Some(arr) = v.get("matches").and_then(|x| x.as_array()) {
        return strings_from_array(arr);
    }
    if let Some(arr) = v.get("results").and_then(|x| x.as_array()) {
        return strings_from_array(arr);
    }
    if let Some(arr) = v.as_array() {
        return strings_from_array(arr);
    }
    if let Some(s) = v.get("message").and_then(|x| x.as_str()) {
        return vec![s.to_string()];
    }
    if let Some(s) = v.as_str() {
        let low = s.to_ascii_lowercase();
        if low.contains("no trap") || low == "clean" || low == "ok" {
            return Vec::new();
        }
        return vec![s.to_string()];
    }
    if v.get("ok").and_then(|x| x.as_bool()) == Some(true)
        && v.get("count").and_then(|x| x.as_u64()) == Some(0)
    {
        return Vec::new();
    }
    Vec::new()
}

fn strings_from_array(arr: &[serde_json::Value]) -> Vec<String> {
    let mut out = Vec::new();
    for it in arr {
        if let Some(s) = it.as_str() {
            out.push(s.to_string());
        } else if let Some(s) = it.get("warning").and_then(|x| x.as_str()) {
            if let Some(proof) = it
                .get("proof")
                .and_then(|x| x.as_str())
                .filter(|p| !p.is_empty())
            {
                out.push(format!("{s} (proof: {proof})"));
            } else {
                out.push(s.to_string());
            }
        } else if let Some(s) = it.get("message").and_then(|x| x.as_str()) {
            out.push(s.to_string());
        } else if let Some(s) = it.get("text").and_then(|x| x.as_str()) {
            out.push(s.to_string());
        } else if let Some(s) = it.get("command").and_then(|x| x.as_str()) {
            if let Some(m) = it.get("reason").and_then(|x| x.as_str()) {
                out.push(format!("{s} — {m}"));
            } else {
                out.push(s.to_string());
            }
        } else if let Some(s) = it.get("path").and_then(|x| x.as_str()) {
            if let Some(m) = it.get("reason").and_then(|x| x.as_str()) {
                out.push(format!("{s} — {m}"));
            } else {
                out.push(s.to_string());
            }
        } else if it.is_object() {
            out.push(it.to_string());
        }
    }
    out.into_iter()
        .filter(|s| {
            let low = s.to_ascii_lowercase();
            !low.is_empty()
                && !(low == "no traps" || low == "clean" || low == "ok" || low == "none")
        })
        .collect()
}

/// Record a lesson in chie memory. Returns true on apparent success, false on
/// offline/unreachable/error (never panics).
pub fn record_lesson(content: &str, kind: Option<&str>, project_dir: Option<&Path>) -> bool {
    record_lesson_with_pin(content, kind, project_dir, None)
}

/// chie's trap guard only matches Rule/Trap/Proof lessons. A bare chie_remember
/// event (the old default) is invisible to check-trap, so wrap the lesson in
/// that shape and give it a matcher the guard can infer (bd-97uwg).
fn rule_lesson_event(content: &str) -> String {
    let bare = strip_lesson_framing(content);
    let matcher = trap_matcher(bare);
    format!(
        "Rule: {bare}\nTrap: `{matcher}` — {bare}\nProof: recorded by flywheel memory record-lesson"
    )
}

fn trap_matcher(content: &str) -> String {
    if let Some(start) = content.find('`')
        && let Some(end) = content[start + 1..].find('`')
    {
        let token = content[start + 1..start + 1 + end].trim();
        if !token.is_empty() {
            return token.to_string();
        }
    }
    let y = content
        .split_once(" breaks when ")
        .or_else(|| content.split_once(" break when "))
        .map(|(_, rest)| rest.split_once("; do ").map(|(y, _)| y).unwrap_or(rest))
        .unwrap_or(content);
    let y = y.trim().trim_end_matches(['.', ';', ',', '!']);
    y.chars().take(120).collect::<String>().trim().to_string()
}

/// Build the `chie memory record` argv for a lesson.
fn build_record_lesson_argv(
    content: &str,
    kind: Option<&str>,
    project_dir: Option<&Path>,
    pin: Option<&str>,
    candidate: bool,
) -> Vec<String> {
    let mut args = vec!["memory".to_string(), "record".to_string()];
    if let Some(dir) = chie_project(project_dir) {
        args.push("--project".to_string());
        args.push(dir);
    }
    // A default kind that chie can derive into a retrievable rule lesson.
    let kind = kind
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("error");
    args.push("--kind".to_string());
    args.push(kind.to_string());
    if let Some(p) = pin.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--pin".to_string());
        args.push(p.to_string());
    }
    if candidate {
        args.push("--candidate".to_string());
    }
    args.push(rule_lesson_event(content));
    args
}

/// Record a lesson in chie memory attributed to an optional agent pin.
pub fn record_lesson_with_pin(
    content: &str,
    kind: Option<&str>,
    project_dir: Option<&Path>,
    pin: Option<&str>,
) -> bool {
    record_lesson_with_options(content, kind, project_dir, pin, false)
}

/// Record a lesson in chie memory with optional agent pin and candidate flag.
pub fn record_lesson_with_options(
    content: &str,
    kind: Option<&str>,
    project_dir: Option<&Path>,
    pin: Option<&str>,
    candidate: bool,
) -> bool {
    let content = content.trim();
    if content.is_empty() {
        return false;
    }
    let argv = build_record_lesson_argv(content, kind, project_dir, pin, candidate);
    let mut cmd = Command::new(chie_bin());
    cmd.args(&argv);
    match run_chie_bounded(&mut cmd, CHIE_SUBPROCESS_TIMEOUT) {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

fn modified_paths(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut paths = Vec::new();
    for line in text.lines() {
        if line.len() <= 3 {
            continue;
        }
        let path = line[3..].trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path).trim_matches('"');
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// One line naming why a check is incomplete; `None` when it ran clean.
pub fn trap_status_note(status: TrapStatus, detail: Option<&str>) -> Option<String> {
    let why = match status {
        TrapStatus::Ok => return None,
        TrapStatus::Offline => "chie is offline",
        TrapStatus::Timeout => "chie timed out",
        TrapStatus::Failed => "chie failed",
    };
    Some(match detail {
        Some(d) => format!("Memory check incomplete ({why}: {d}); known traps may be missing."),
        None => format!("Memory check incomplete ({why}); known traps may be missing."),
    })
}

/// Render the briefing memory-traps section (markdown). A clean check with no
/// traps renders nothing; an incomplete one always says so.
pub fn render_traps_section(check: &TrapCheck) -> String {
    let note = trap_status_note(check.status, check.detail.as_deref());
    if check.traps.is_empty() && note.is_none() {
        return String::new();
    }
    let mut s = String::from("### Chie Memory Traps & Invariants\n");
    for t in &check.traps {
        s.push_str("- ");
        s.push_str(t);
        s.push('\n');
    }
    if let Some(note) = note {
        s.push_str("- ");
        s.push_str(&note);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "flywheel-memory-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fake_chie(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("chie");
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn with_chie_bin<T>(bin: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::config::lock_env();
        let old = std::env::var_os("FLYWHEEL_CHIE_BIN");
        unsafe { std::env::set_var("FLYWHEEL_CHIE_BIN", bin) };
        let out = f();
        match old {
            Some(v) => unsafe { std::env::set_var("FLYWHEEL_CHIE_BIN", v) },
            None => unsafe { std::env::remove_var("FLYWHEEL_CHIE_BIN") },
        }
        out
    }

    #[test]
    fn lesson_core_dedups_old_and_new_chie_shapes() {
        let old = "Rule: Rule: Rule: a zebra marker breaks when a dedup probe runs; do keep one lesson Trap: `a dedup probe runs` — Rule: Rule: a zebra marker breaks when a dedup probe runs; do keep one lesson Proof: recorded by flywheel memory record-lesson";
        let new = "Rule: Rule: a zebra marker breaks when a dedup probe runs; do keep one lesson: `a dedup probe runs` — Rule: Rule: a zebra marker breaks when a dedup probe runs; (proof: recorded by flywheel memory record-lesson)";
        assert_eq!(lesson_core(old), lesson_core(new));
        assert_eq!(
            lesson_core(old),
            "a zebra marker breaks when a dedup probe runs; do keep one lesson"
        );
    }

    #[test]
    fn lesson_core_keeps_backticks_inside_the_rule_text() {
        let a = "Rule: use `a` breaks when a dedup probe runs; do keep alpha";
        let b = "Rule: use `b` breaks when a dedup probe runs; do keep beta";
        assert_ne!(lesson_core(a), lesson_core(b));
        assert!(lesson_core(a).contains("`a`"), "{}", lesson_core(a));
    }

    #[test]
    fn rule_lesson_event_sends_a_bare_lesson() {
        let event = rule_lesson_event("Rule: Rule: remember this; do that");
        assert!(
            event.starts_with("Rule: remember this; do that\n"),
            "{event}"
        );
        assert!(!event.contains("Rule: Rule:"), "{event}");
    }

    fn argv_logging_chie(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
        let log = dir.join("argv.log");
        let fake = write_fake_chie(
            dir,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\n",
                log.display()
            ),
        );
        (fake, log)
    }

    fn git_dir_with_change(name: &str, file: &str) -> PathBuf {
        let dir = tmpdir(name);
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "git init");
        let path = dir.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
        dir
    }

    #[test]
    fn check_trap_offline_missing_chie_says_offline() {
        let dir = tmpdir("offline");
        let missing = dir.join("chie-missing");
        let check = with_chie_bin(&missing, || check_trap("cargo test", None));
        assert!(check.traps.is_empty(), "{check:?}");
        assert_eq!(check.status, TrapStatus::Offline);
    }

    #[test]
    fn check_trap_daemon_unreachable_says_offline() {
        let dir = tmpdir("unreachable");
        let fake = write_fake_chie(&dir, "#!/bin/sh\necho 'connect refused' >&2\nexit 1\n");
        let check = with_chie_bin(&fake, || check_trap("cargo test", None));
        assert!(check.traps.is_empty(), "{check:?}");
        assert_eq!(check.status, TrapStatus::Offline);
    }

    #[test]
    fn check_trap_is_one_call_with_query_command_and_path() {
        let dir = tmpdir("one-call");
        let (fake, log) = argv_logging_chie(
            &dir,
            r#"printf '[{"rule_id":"e-1","warning":"keep leases exact"}]'"#,
        );
        let check = with_chie_bin(&fake, || check_trap("src/cmd/spawn.rs", None));
        assert_eq!(check.traps, vec!["keep leases exact".to_string()]);
        assert_eq!(check.status, TrapStatus::Ok);
        let argv = fs::read_to_string(log).unwrap();
        let calls: Vec<&str> = argv.lines().collect();
        assert_eq!(
            calls,
            vec![
                "memory check-trap --query src/cmd/spawn.rs --command src/cmd/spawn.rs --path src/cmd/spawn.rs --json"
            ]
        );
    }

    #[test]
    fn briefing_sends_one_query_and_one_check_per_path_and_never_recalls() {
        let dir = git_dir_with_change("brief-calls", "src/lease.rs");
        let (fake, log) = argv_logging_chie(&tmpdir("brief-calls-bin"), "printf '[]'");
        let check = with_chie_bin(&fake, || {
            briefing_traps("fix the lease drain", Some("bd-123"), Some(&dir))
        });
        assert_eq!(check.status, TrapStatus::Ok, "{check:?}");
        let argv = fs::read_to_string(log).unwrap();
        assert!(!argv.contains("recall"), "no recall in preflight: {argv}");
        assert_eq!(argv.lines().count(), 2, "one query + one path: {argv}");
        assert!(
            argv.lines()
                .any(|l| l.ends_with("--query fix the lease drain bd-123 src/lease.rs --json")),
            "{argv}"
        );
        assert!(
            argv.lines()
                .any(|l| l.ends_with("--path src/lease.rs --json")),
            "{argv}"
        );
    }

    #[test]
    fn a_hung_path_check_keeps_the_query_hits_and_says_timeout() {
        let dir = git_dir_with_change("brief-partial", "src/slow.rs");
        let fake = write_fake_chie(
            &tmpdir("brief-partial-bin"),
            r#"#!/bin/sh
case "$*" in
  *--path*) exec sleep 5 ;;
esac
printf '[{"rule_id":"e-1","warning":"drain parks are bounded"}]'
"#,
        );
        let check = with_chie_bin(&fake, || briefing_traps("drain", None, Some(&dir)));
        assert_eq!(check.traps, vec!["drain parks are bounded".to_string()]);
        assert_eq!(check.status, TrapStatus::Timeout);
        let section = render_traps_section(&check);
        assert!(section.contains("drain parks are bounded"), "{section}");
        assert!(section.contains("Memory check incomplete"), "{section}");
    }

    #[test]
    fn invalid_check_trap_json_is_a_failure_not_clean() {
        let dir = tmpdir("bad-json");
        let fake = write_fake_chie(&dir, "#!/bin/sh\nprintf 'not json'\n");
        let check = with_chie_bin(&fake, || check_trap("cargo test", None));
        assert_eq!(check.status, TrapStatus::Failed);
        assert!(
            check
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("invalid JSON"),
            "{check:?}"
        );
    }

    #[test]
    fn record_lesson_offline_missing_chie_is_graceful() {
        let dir = tmpdir("record-offline");
        let missing = dir.join("chie-missing");
        let ok = with_chie_bin(&missing, || {
            record_lesson("remember this", Some("decision"), None)
        });
        assert!(!ok, "missing chie cannot record, but must not panic");
    }

    #[test]
    fn extract_trap_lines_json_shapes() {
        let lines = |raw: &str| extract_trap_lines_json(&serde_json::from_str(raw).unwrap());
        assert!(lines(r#"{"traps":[]}"#).is_empty());
        assert!(lines(r#"{"matches":[]}"#).is_empty());
        assert_eq!(
            lines(r#"{"traps":[{"message":"do not rm -rf"}]}"#),
            vec!["do not rm -rf".to_string()]
        );
        assert_eq!(
            lines(r#"{"matches":["watch the lease"]}"#),
            vec!["watch the lease".to_string()]
        );
        assert_eq!(
            lines(r#"["alpha","beta"]"#),
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(
            lines(
                r#"[{"rule_id":"e-1","warning":"Strip TMPDIR trailing slash","proof":"proof46"}]"#
            ),
            vec!["Strip TMPDIR trailing slash (proof: proof46)".to_string()]
        );
    }

    fn check_of(traps: &[&str], status: TrapStatus) -> TrapCheck {
        TrapCheck {
            traps: traps.iter().map(|t| t.to_string()).collect(),
            status,
            detail: None,
        }
    }

    #[test]
    fn render_traps_section_formats_markdown() {
        assert_eq!(render_traps_section(&check_of(&[], TrapStatus::Ok)), "");
        let s = render_traps_section(&check_of(&["x", "y"], TrapStatus::Ok));
        assert!(s.starts_with("### Chie Memory Traps & Invariants\n"));
        assert!(s.contains("- x\n"));
        assert!(s.contains("- y\n"));
        assert!(!s.contains("incomplete"), "{s}");
    }

    #[test]
    fn an_offline_check_never_renders_as_clean() {
        let s = render_traps_section(&check_of(&[], TrapStatus::Offline));
        assert!(
            s.contains("Memory check incomplete (chie is offline)"),
            "{s}"
        );
    }

    #[test]
    fn the_worst_call_status_wins() {
        let check = fold_outcomes(vec![
            CallOutcome::Found(vec!["a".into()]),
            CallOutcome::Offline,
            CallOutcome::Timeout("slow".into()),
            CallOutcome::Found(vec!["a".into(), "b".into()]),
        ]);
        assert_eq!(check.status, TrapStatus::Timeout);
        assert_eq!(check.traps, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(check.detail.as_deref(), Some("slow"));
    }

    #[test]
    fn modified_paths_reads_git_porcelain_shape() {
        let dir = tmpdir("nogit");
        assert!(modified_paths(&dir).is_empty());
    }

    #[test]
    fn briefing_traps_offline_is_empty_and_says_offline() {
        let dir = tmpdir("brief-offline");
        let missing = dir.join("chie-missing");
        let check = with_chie_bin(&missing, || {
            briefing_traps("fix bd-123 in src/main.rs", Some("bd-123"), Some(&dir))
        });
        assert!(check.traps.is_empty(), "{check:?}");
        assert_eq!(check.status, TrapStatus::Offline);
    }

    #[test]
    fn run_chie_bounded_kills_hung_subprocess() {
        let dir = tmpdir("timeout");
        let slow = write_fake_chie(&dir, "#!/bin/sh\nexec sleep 5\n");
        let start = std::time::Instant::now();
        let err = run_chie_bounded(&mut Command::new(slow), Duration::from_millis(80))
            .expect_err("hung chie should time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn record_lesson_with_pin_forwards_pin_flag() {
        let dir = tmpdir("record-pin");
        let log = dir.join("args.txt");
        let fake = write_fake_chie(
            &dir,
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\n", log.display()),
        );
        let ok = with_chie_bin(&fake, || {
            record_lesson_with_pin(
                "Avoid trailing slash",
                Some("error"),
                None,
                Some("QuietLantern"),
            )
        });
        assert!(ok);
        let recorded = fs::read_to_string(log).unwrap();
        assert!(recorded.contains("--pin QuietLantern"), "args: {recorded}");
        assert!(!recorded.contains("--peer"), "args: {recorded}");
    }

    fn serve_once(status_line: &str, body: &str, delay: Duration) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let status_line = status_line.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn briefing_traps_uses_http_fast_path_and_caps_to_five() {
        let traps: Vec<_> = (1..=8)
            .map(|i| serde_json::json!({ "rule_id": format!("r-{i}"), "warning": format!("Rule {i}") }))
            .collect();
        let url = serve_once(
            "200 OK",
            &serde_json::json!({ "ok": true, "traps": traps, "count": 8 }).to_string(),
            Duration::ZERO,
        );
        let dir = tmpdir("http-200-traps");
        let missing = dir.join("chie-missing");
        let check = with_chie_bin(&missing, || {
            briefing_traps_with_http_url(Some(&url), "fix bug", None, Some(&dir))
        });
        assert_eq!(check.status, TrapStatus::Ok);
        assert_eq!(check.traps.len(), MAX_BRIEF_TRAPS);
    }

    #[test]
    fn briefing_traps_http_200_empty_skips_subprocess_fallback() {
        let url = serve_once(
            "200 OK",
            r#"{"ok":true,"traps":[],"count":0}"#,
            Duration::ZERO,
        );
        let dir = tmpdir("http-200-empty");
        let missing = dir.join("chie-missing");
        let check = with_chie_bin(&missing, || {
            briefing_traps_with_http_url(Some(&url), "fix bug", None, Some(&dir))
        });
        assert_eq!(check.status, TrapStatus::Ok);
        assert!(check.traps.is_empty());
    }

    #[test]
    fn briefing_traps_http_404_falls_back_to_subprocess() {
        let url = serve_once("404 Not Found", r#"{"error":"not found"}"#, Duration::ZERO);
        let dir = tmpdir("http-404-fallback");
        let missing = dir.join("chie-missing");
        let check = with_chie_bin(&missing, || {
            briefing_traps_with_http_url(Some(&url), "fix bug", None, Some(&dir))
        });
        assert_eq!(check.status, TrapStatus::Offline);
    }

    #[test]
    fn briefing_traps_http_timeout_returns_timeout_status() {
        let url = serve_once(
            "200 OK",
            r#"{"ok":true,"traps":[],"count":0}"#,
            Duration::from_millis(450),
        );
        let dir = tmpdir("http-timeout");
        let check = briefing_traps_with_http_url(Some(&url), "fix bug", None, Some(&dir));
        assert_eq!(check.status, TrapStatus::Timeout);
    }

    #[test]
    fn build_record_lesson_argv_includes_candidate_only_when_requested() {
        let active = build_record_lesson_argv("Avoid X", Some("decision"), None, None, false);
        assert!(!active.iter().any(|a| a == "--candidate"));
        let cand =
            build_record_lesson_argv("Avoid X", Some("decision"), None, Some("AmberFalcon"), true);
        assert!(cand.iter().any(|a| a == "--candidate"));
        assert!(cand.windows(2).any(|w| w == ["--pin", "AmberFalcon"]));
    }
}

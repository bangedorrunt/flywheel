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
            json,
        } => cmd_record_lesson(
            cfg,
            content,
            kind.as_deref(),
            project.as_deref(),
            r#as.as_deref(),
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
    let traps = check_trap_queries_strict(&[query.to_string()], project_dir.as_deref())?;
    if json {
        print_json(&serde_json::json!({
            "ok": true,
            "query": query,
            "traps": traps,
            "count": traps.len(),
        }));
    } else if traps.is_empty() {
        println!("no traps");
    } else {
        for t in &traps {
            println!("- {t}");
        }
    }
    Ok(())
}

fn cmd_record_lesson(
    cfg: &Config,
    content: &str,
    kind: Option<&str>,
    project: Option<&str>,
    as_agent: Option<&str>,
    json: bool,
) -> Result<()> {
    let project_dir = project.map(|p| crate::project_dir(cfg, p));
    let recorded = record_lesson_with_pin(content, kind, project_dir.as_deref(), as_agent);
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

/// Collect trap/invariant lines for briefing preflight. Queries are matched
/// against chie memory traps (commands/paths/text) and modified paths when a
/// project dir is provided. Offline => empty.
pub fn briefing_traps(
    objective: &str,
    project: Option<&str>,
    bead: Option<&str>,
    project_dir: Option<&Path>,
) -> Vec<String> {
    let mut queries: Vec<String> = Vec::new();
    let obj = objective.trim();
    if !obj.is_empty() {
        queries.push(obj.to_string());
        for tok in obj.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
            if tok.starts_with("bd-") {
                queries.push(tok.to_string());
            }
        }
    }
    if let Some(b) = bead.map(str::trim).filter(|s| !s.is_empty()) {
        queries.push(b.to_string());
    }
    let _ = project; // reserved for future scoping; dir carries the scope today.
    if let Some(dir) = project_dir {
        for p in modified_paths(dir).into_iter().take(12) {
            queries.push(p);
        }
    }
    queries.sort();
    queries.dedup();
    check_trap_queries(&queries, project_dir)
}

/// Briefing preflight view. An unreachable chie is clean; a usage error is
/// logged and downgraded here so a briefing never fails on a broken tool.
pub fn check_trap_queries(queries: &[String], project_dir: Option<&Path>) -> Vec<String> {
    check_trap_queries_strict(queries, project_dir).unwrap_or_else(|e| {
        eprintln!("flywheel: memory trap check unavailable: {e}");
        Vec::new()
    })
}

/// The strict check used by `flywheel memory check-trap`: a chie usage error is
/// a loud failure, never a silent "no traps".
pub fn check_trap_queries_strict(
    queries: &[String],
    project_dir: Option<&Path>,
) -> Result<Vec<String>> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for q in queries {
        let q = q.trim();
        if q.is_empty() {
            continue;
        }
        let path_like =
            q.contains('/') || q.ends_with(".rs") || q.ends_with(".toml") || q.ends_with(".md");
        if path_like {
            merge_lines(
                &mut out,
                run_check_trap(&["--path", q, "--json"], project_dir)?,
            );
        }
        merge_lines(
            &mut out,
            run_check_trap_with_query_fallback(q, project_dir)?,
        );
        // The trap guard matches exact argv/path tokens; migrated lessons name
        // their file slug, so natural prose queries never hit. Recall over the
        // same Rule/Trap lessons is the general read path (bd-ndbnv).
        for line in lesson_recall(q, project_dir)? {
            let line = normalize_line(&line);
            if !line.is_empty() {
                out.insert(line.chars().take(240).collect());
            }
        }
    }
    if !queries.is_empty() {
        let joined = queries.join(" ");
        let clipped: String = joined.chars().take(400).collect();
        merge_lines(
            &mut out,
            run_check_trap_with_query_fallback(&clipped, project_dir)?,
        );
    }
    // Both the trap guard and recall can surface the same lesson several
    // times (re-records, derived conclusions). Canonicalize once at the edge.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut rows = Vec::new();
    for line in out {
        let key: String = line
            .trim()
            .trim_start_matches("Rule: ")
            .trim()
            .chars()
            .take(80)
            .collect();
        if seen.insert(key) {
            rows.push(line);
        }
    }
    Ok(rows.into_iter().take(20).collect())
}

fn merge_lines(out: &mut BTreeSet<String>, raw: Option<String>) {
    let Some(raw) = raw else { return };
    for line in extract_trap_lines(&raw) {
        let line = normalize_line(&line);
        if !line.is_empty() {
            out.insert(line);
        }
    }
}

fn normalize_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

fn run_check_trap_with_query_fallback(
    query: &str,
    project_dir: Option<&Path>,
) -> Result<Option<String>> {
    match run_check_trap(
        &["--command", query, "--query", query, "--json"],
        project_dir,
    ) {
        Ok(res) => Ok(res),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("unexpected argument") && msg.contains("--query") {
                run_check_trap(&["--command", query, "--json"], project_dir)
            } else {
                Err(err)
            }
        }
    }
}

fn run_check_trap(args: &[&str], project_dir: Option<&Path>) -> Result<Option<String>> {
    let mut cmd = Command::new(chie_bin());
    cmd.args(["memory", "check-trap"]);
    if let Some(dir) = chie_project(project_dir) {
        cmd.args(["--project", &dir]);
    }
    cmd.args(args);
    let out = match run_chie_bounded(&mut cmd, CHIE_SUBPROCESS_TIMEOUT) {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("cannot run chie check-trap: {e}"),
    };
    if out.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ));
    }
    let err_text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if chie_unreachable(&err_text) {
        return Ok(None);
    }
    let detail = if err_text.is_empty() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        err_text
    };
    bail!(
        "chie memory check-trap failed ({}): {}",
        args.join(" "),
        detail
    )
}

/// Recall Rule/Trap lessons by text — the read path for lessons whose trap
/// matcher is a slug rather than the natural phrase a preflight query uses.
fn lesson_recall(query: &str, project_dir: Option<&Path>) -> Result<Vec<String>> {
    let mut cmd = Command::new(chie_bin());
    cmd.args(["memory", "recall", query, "--json"]);
    if let Some(dir) = chie_project(project_dir) {
        cmd.args(["--project", &dir]);
    }
    let out = match run_chie_bounded(&mut cmd, CHIE_SUBPROCESS_TIMEOUT) {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("cannot run chie recall: {e}"),
    };
    if !out.status.success() {
        let err_text = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if chie_unreachable(&err_text) {
            return Ok(Vec::new());
        }
        bail!("chie memory recall failed ({query}): {err_text}");
    }
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| anyhow::anyhow!("chie recall returned invalid JSON: {e}"))?;
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for key in ["conclusions", "events"] {
        if let Some(arr) = doc.get(key).and_then(serde_json::Value::as_array) {
            for item in arr {
                if let Some(content) = item.get("content").and_then(serde_json::Value::as_str)
                    && (content.contains("Rule:") || content.contains("Trap:"))
                {
                    // Re-recorded lessons leave duplicate events; the preflight
                    // view must show each lesson once (bd-ndbnv review point 4).
                    let canon = content
                        .trim()
                        .trim_start_matches("Rule: ")
                        .trim()
                        .to_string();
                    if seen.insert(canon) {
                        rows.push(content.to_string());
                    }
                }
            }
        }
    }
    Ok(rows)
}

fn extract_trap_lines(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return extract_trap_lines_json(&v);
    }
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            !(low == "no traps" || low == "clean" || low == "ok" || low == "none")
        })
        .map(|l| l.trim_start_matches('-').trim().to_string())
        .collect()
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
    let matcher = trap_matcher(content);
    format!(
        "Rule: {content}\nTrap: `{matcher}` — {content}\nProof: recorded by flywheel memory record-lesson"
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

/// Record a lesson in chie memory attributed to an optional agent pin.
pub fn record_lesson_with_pin(
    content: &str,
    kind: Option<&str>,
    project_dir: Option<&Path>,
    pin: Option<&str>,
) -> bool {
    let content = content.trim();
    if content.is_empty() {
        return false;
    }
    let mut cmd = Command::new(chie_bin());
    cmd.args(["memory", "record"]);
    if let Some(dir) = chie_project(project_dir) {
        cmd.args(["--project", &dir]);
    }
    // A default kind that chie can derive into a retrievable rule lesson.
    let kind = kind
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("error");
    cmd.args(["--kind", kind]);
    if let Some(p) = pin.map(str::trim).filter(|s| !s.is_empty()) {
        // chie record has --peer, not --pin (bd-97uwg).
        cmd.args(["--peer", &format!("agent:{p}")]);
    }
    cmd.arg(rule_lesson_event(content));
    match run_chie_bounded(&mut cmd, CHIE_SUBPROCESS_TIMEOUT) {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

fn modified_paths(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
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

/// Render the briefing memory-traps section (markdown). Empty => empty string.
pub fn render_traps_section(traps: &[String]) -> String {
    if traps.is_empty() {
        return String::new();
    }
    let mut s = String::from("### Chie Memory Traps & Invariants\n");
    for t in traps {
        s.push_str("- ");
        s.push_str(t);
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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    fn check_trap_offline_missing_chie_is_clean() {
        let dir = tmpdir("offline");
        let missing = dir.join("chie-missing");
        let traps = with_chie_bin(&missing, || {
            check_trap_queries(&["cargo test".to_string()], None)
        });
        assert!(traps.is_empty(), "offline check must be clean: {traps:?}");
    }

    #[test]
    fn check_trap_daemon_unreachable_is_clean() {
        let dir = tmpdir("unreachable");
        let fake = write_fake_chie(&dir, "#!/bin/sh\necho 'connect refused' >&2\nexit 1\n");
        let traps = with_chie_bin(&fake, || {
            check_trap_queries(&["cargo test".to_string()], None)
        });
        assert!(
            traps.is_empty(),
            "daemon-unreachable check must be clean: {traps:?}"
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
        assert!(extract_trap_lines(r#"{"traps":[]}"#).is_empty());
        assert!(extract_trap_lines(r#"{"matches":[]}"#).is_empty());
        assert!(extract_trap_lines("no traps").is_empty());
        let a = extract_trap_lines(r#"{"traps":[{"message":"do not rm -rf"}]}"#);
        assert_eq!(a, vec!["do not rm -rf".to_string()]);
        let b = extract_trap_lines(r#"{"matches":["watch the lease"]}"#);
        assert_eq!(b, vec!["watch the lease".to_string()]);
        let c = extract_trap_lines(r#"["alpha","beta"]"#);
        assert_eq!(c, vec!["alpha".to_string(), "beta".to_string()]);
        let d = extract_trap_lines(
            r#"[{"rule_id":"e-1","warning":"Strip TMPDIR trailing slash","proof":"proof46"}]"#,
        );
        assert_eq!(
            d,
            vec!["Strip TMPDIR trailing slash (proof: proof46)".to_string()]
        );
    }

    #[test]
    fn render_traps_section_formats_markdown() {
        assert_eq!(render_traps_section(&[]), "");
        let s = render_traps_section(&["x".to_string(), "y".to_string()]);
        assert!(s.starts_with("### Chie Memory Traps & Invariants\n"));
        assert!(s.contains("- x\n"));
        assert!(s.contains("- y\n"));
    }

    #[test]
    fn modified_paths_reads_git_porcelain_shape() {
        let dir = tmpdir("nogit");
        assert!(modified_paths(&dir).is_empty());
    }

    #[test]
    fn briefing_traps_offline_is_empty() {
        let dir = tmpdir("brief-offline");
        let missing = dir.join("chie-missing");
        let traps = with_chie_bin(&missing, || {
            briefing_traps(
                "fix bd-123 in src/main.rs",
                Some("flywheel"),
                Some("bd-123"),
                Some(&dir),
            )
        });
        assert!(
            traps.is_empty(),
            "offline briefing preflight must be empty: {traps:?}"
        );
    }

    #[test]
    fn check_trap_falls_back_when_query_flag_rejected() {
        let dir = tmpdir("query-fallback");
        let fake = write_fake_chie(
            &dir,
            r#"#!/bin/sh
if [ "$2" = "recall" ]; then
  printf '{"conclusions":[],"events":[]}'
  exit 0
fi
if echo "$*" | grep -q -- "--query"; then
  echo "error: unexpected argument '--query' found" >&2
  exit 2
fi
printf '[{"rule_id":"r-1","warning":"Strip TMPDIR slash"}]'
"#,
        );
        let traps = with_chie_bin(&fake, || {
            check_trap_queries_strict(&["proof46".to_string()], None).unwrap()
        });
        assert_eq!(traps, vec!["Strip TMPDIR slash".to_string()]);
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
    fn record_lesson_with_pin_forwards_peer_flag() {
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
        assert!(
            recorded.contains("--peer agent:QuietLantern"),
            "args: {recorded}"
        );
    }
}

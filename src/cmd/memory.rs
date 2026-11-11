//! `flywheel memory` — Chie Memory trap preflight and lesson recording.
//!
//! Surfaces known traps/invariants at briefing/claim time and records lessons
//! back into the memory plane. All chie calls degrade gracefully when the
//! daemon is absent or unreachable: preflight returns empty, never panics.

use anyhow::Result;
use clap::Subcommand;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

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
    let traps = check_trap_queries(&[query.to_string()], project_dir.as_deref());
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
    _as_agent: Option<&str>,
    json: bool,
) -> Result<()> {
    let project_dir = project.map(|p| crate::project_dir(cfg, p));
    let recorded = record_lesson(content, kind, project_dir.as_deref());
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

/// Run `chie memory check-trap` over queries. Returns normalized trap lines.
/// Any spawn/parse/daemon error => empty (offline fallback).
pub fn check_trap_queries(queries: &[String], project_dir: Option<&Path>) -> Vec<String> {
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
                run_check_trap(&["--path", q, "--json"], project_dir),
            );
        }
        merge_lines(
            &mut out,
            run_check_trap(&["--command", q, "--json"], project_dir),
        );
    }
    if !queries.is_empty() {
        let joined = queries.join(" ");
        let clipped: String = joined.chars().take(400).collect();
        merge_lines(
            &mut out,
            run_check_trap(&["--command", &clipped, "--json"], project_dir),
        );
    }
    out.into_iter().take(20).collect()
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

fn run_check_trap(args: &[&str], project_dir: Option<&Path>) -> Option<String> {
    let mut cmd = Command::new(chie_bin());
    cmd.args(["memory", "check-trap"]);
    if let Some(dir) = project_dir {
        cmd.args(["--project", &dir.to_string_lossy()]);
    }
    cmd.args(args);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let err = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
        if text.is_empty()
            && (err.contains("connect")
                || err.contains("unreachable")
                || err.contains("refused")
                || err.contains("daemon"))
        {
            return None;
        }
        if text.is_empty() {
            return Some(String::new());
        }
        return Some(text);
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
    if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
        if let Some(n) = v.get("count").and_then(|x| x.as_u64()) {
            if n == 0 {
                return Vec::new();
            }
        }
    }
    Vec::new()
}

fn strings_from_array(arr: &[serde_json::Value]) -> Vec<String> {
    let mut out = Vec::new();
    for it in arr {
        if let Some(s) = it.as_str() {
            out.push(s.to_string());
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
            !low.is_empty() && !(low == "no traps" || low == "clean" || low == "ok" || low == "none")
        })
        .collect()
}

/// Record a lesson in chie memory. Returns true on apparent success, false on
/// offline/unreachable/error (never panics).
pub fn record_lesson(content: &str, kind: Option<&str>, project_dir: Option<&Path>) -> bool {
    let content = content.trim();
    if content.is_empty() {
        return false;
    }
    let mut cmd = Command::new(chie_bin());
    cmd.args(["memory", "record"]);
    if let Some(dir) = project_dir {
        cmd.args(["--project", &dir.to_string_lossy()]);
    }
    if let Some(k) = kind.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.args(["--kind", k]);
    }
    cmd.arg(content);
    match cmd.output() {
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
        let _guard = ENV_LOCK.lock().unwrap();
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
}

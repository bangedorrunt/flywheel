//! One typed `br` client (wave-3): every flywheel `br` subprocess goes
//! through this module — one spawn, one error shape, one row mapping.
//! Callers never shell `br` directly and never re-derive the JSON shapes.
//!
//! `FLYWHEEL_BR_BIN` pins an alternate `br` binary (tests point it at a
//! fixture `br` on PATH); unset means plain `br`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The `br` binary: `$FLYWHEEL_BR_BIN` when set and non-blank, else `br`.
/// Shape-checked through the strict helper (accepted: a non-empty path);
/// any set value is a program name, so this cannot fail in practice.
pub fn br_program() -> String {
    crate::config::parse_path("FLYWHEEL_BR_BIN")
        .unwrap_or(None)
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "br".to_string())
}

/// Run `br` with `args` in `dir` (None = inherit cwd); `Ok(stdout)` on exit 0.
/// One spawn, one error shape: spawn failure names the binary, a non-zero
/// exit names the argv plus trimmed stderr.
pub fn run_br(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    run_br_with_env(dir, args, &[])
}

/// [`run_br`] with extra per-command env (e.g. test-only operator flags).
/// Same single spawn site, same error shape.
pub fn run_br_with_env(dir: Option<&Path>, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = std::process::Command::new(br_program());
    cmd.args(args);
    cmd.envs(env.iter().copied());
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("br not found: {e}"))?;
    if !out.status.success() {
        bail!(
            "br {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `br list`-family envelope: a bare array or an object carrying it under
/// `issues`. Anything else is refused (moved verbatim from main.rs —
/// same messages, same tolerance).
pub fn parse_issue_rows_json(raw: &str, what: &str) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow::anyhow!("{what} bad JSON: {e}: {raw}"))?;
    match &v {
        Value::Array(items) => Ok(items.clone()),
        Value::Object(obj) => {
            let issues = obj.get("issues").cloned().unwrap_or_default();
            anyhow::ensure!(
                issues.is_array(),
                "{what}: unrecognized object shape (expected issues array): {raw}"
            );
            Ok(issues.as_array().cloned().unwrap_or_default())
        }
        _ => bail!("{what}: expected array or issues object: {raw}"),
    }
}

/// Shared `ReadyBead` row mapping (moved verbatim from main.rs): empty
/// strings become absent, `priority` clamps u64→u8 (default 3), typed `wave`
/// wins, principles format via `durable` helpers.
pub fn ready_bead_from_row(b: &Value) -> crate::durable::ReadyBead {
    crate::durable::ReadyBead {
        id: b
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        title: b
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        assignee: b
            .get("assignee")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        verify: b
            .get("verify")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        labels: b
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        wave: crate::durable::bead_wave_field(b),
        priority: b
            .get("priority")
            .and_then(Value::as_u64)
            .map(|p| p.min(u64::from(u8::MAX)) as u8)
            .unwrap_or(3),
        principles: crate::durable::principles_typed_field(b),
        ..Default::default()
    }
}

/// `br ready --json` (moved verbatim from main.rs: array or `{issues}`
/// wrapper, same error strings).
pub fn parse_ready_beads_json(raw: &str) -> Result<Vec<crate::durable::ReadyBead>> {
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow::anyhow!("br ready bad JSON: {e}: {raw}"))?;
    let beads: Vec<Value> = match &v {
        Value::Array(items) => items.clone(),
        Value::Object(obj) => {
            let issues = obj.get("issues").cloned().unwrap_or_default();
            anyhow::ensure!(
                issues.is_array(),
                "br ready JSON: unrecognized object shape (expected issues array): {raw}"
            );
            issues.as_array().cloned().unwrap_or_default()
        }
        _ => anyhow::bail!("br ready JSON: expected array or issues object: {raw}"),
    };
    Ok(beads.iter().map(ready_bead_from_row).collect())
}

/// `br list --status in_progress --json` (moved verbatim from main.rs).
pub fn parse_inprogress_beads_json(raw: &str) -> Result<Vec<crate::durable::ReadyBead>> {
    let v: Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow::anyhow!("br list bad JSON: {e}: {raw}"))?;
    let list: &Vec<Value> = v
        .get("issues")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("br list output has no issues array: {raw}"))?;
    Ok(list.iter().map(ready_bead_from_row).collect())
}

/// One typed `br show` row: exactly the fields flywheel's show call sites
/// read (planner routing reads `created_by`; the close report reads `title`
/// and the latest comment). Array-first-or-object normalization matches the
/// old inline code. Rationale: current `br show` emits a bare object, so an
/// array-only read would silently empty every report.
#[derive(Debug, Clone, Default)]
pub struct Bead {
    pub title: String,
    pub created_by: Option<String>,
    pub comments: Vec<BeadComment>,
}

/// One `comments[]` row (only `text` is read).
#[derive(Debug, Clone, Default)]
pub struct BeadComment {
    pub text: String,
}

/// Latest comment text, trimmed (the close reason lands as a comment).
pub fn latest_comment_text(bead: &Bead) -> String {
    bead.comments
        .last()
        .map(|c| c.text.trim().to_string())
        .unwrap_or_default()
}

fn bead_from_row(b: &Value) -> Bead {
    Bead {
        title: b
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        created_by: b
            .get("created_by")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        comments: b
            .get("comments")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        c.get("text").and_then(Value::as_str).map(|t| BeadComment {
                            text: t.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

impl Bead {
    /// `br show <id> --json`: first row of an array response, or the object
    /// itself (same normalization the call sites did inline).
    pub fn show(dir: Option<&Path>, id: &str) -> Result<Bead> {
        let raw = run_br(dir, &["show", id, "--json"])?;
        let v: Value =
            serde_json::from_str(raw.trim()).with_context(|| format!("br show {id} bad JSON"))?;
        let row = match &v {
            Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
            obj @ Value::Object(_) => obj.clone(),
            _ => Value::Null,
        };
        Ok(bead_from_row(&row))
    }

    /// `br show` existence probe (no output captured, exit code only).
    pub fn exists(dir: Option<&Path>, id: &str) -> bool {
        let mut cmd = std::process::Command::new(br_program());
        cmd.args(["show", id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    /// `br ready --json` as typed rows.
    pub fn ready(dir: Option<&Path>) -> Result<Vec<crate::durable::ReadyBead>> {
        let raw = run_br(dir, &["ready", "--json"])?;
        parse_ready_beads_json(&raw)
    }

    /// `br list --status in_progress --json` as typed rows.
    pub fn in_progress(dir: Option<&Path>) -> Result<Vec<crate::durable::ReadyBead>> {
        let raw = run_br(dir, &["list", "--status", "in_progress", "--json"])?;
        parse_inprogress_beads_json(&raw)
    }

    /// `br create … --json` → the created id. Accepts br's id shapes
    /// (`{issue:{id}}`, `{created:{id}}` when warnings fire, `{id}`,
    /// `[{id}]`); unparseable output degrades to `"(id unknown)"` exactly
    /// as the cron/awareness call sites did.
    pub fn create(dir: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut full: Vec<&str> = args.to_vec();
        full.push("--json");
        let raw = run_br(dir, &full)?;
        let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
        let node = v
            .get("issue")
            .or_else(|| v.get("created"))
            .or_else(|| v.get("id"))
            .or_else(|| v.as_array().and_then(|a| a.first()))
            .cloned()
            .unwrap_or(Value::Null);
        // The id itself when the envelope IS the id string (`{"id":"bd-x"}`
        // yields the string after the first deref); else the id field.
        if let Some(s) = node.as_str() {
            return Ok(s.to_string());
        }
        Ok(node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("(id unknown)")
            .to_string())
    }

    /// `br list --json` issues rows (for callers that walk arbitrary fields).
    pub fn list_issues(dir: Option<&Path>, extra: &[&str], what: &str) -> Result<Vec<Value>> {
        let mut full: Vec<&str> = vec!["list"];
        full.extend_from_slice(extra);
        full.push("--json");
        let raw = run_br(dir, &full)?;
        parse_issue_rows_json(&raw, what)
    }
}

/// `br status` substring classifier (moved verbatim from coalition.rs):
/// closed → closed, open/in_progress → open, else unknown.
pub fn br_status_of_stdout(stdout: &str) -> &str {
    if stdout.contains("\"status\":\"closed\"") {
        "closed"
    } else if stdout.contains("\"status\":\"open\"")
        || stdout.contains("\"status\":\"in_progress\"")
    {
        "open"
    } else {
        "unknown"
    }
}

/// Shared test fixture: git init + `br init` + policy + `br create` +
/// `br list` → bead id, with an optional real commit. Collapses the four
/// copied scratch-workspace blocks (main.rs verifier/loop tests + verdict
/// tests) into one implementation. A `None` verify models a triage/dupe
/// bead via `br q` (no VERIFY fence); `commit_prefix` writes
/// `feat: {prefix} ({id})` with test git identity.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn scratch_bead(
    dir: &std::path::Path,
    policy: &str,
    title: &str,
    slug: &str,
    priority: &str,
    verify: Option<&str>,
    principle: Option<&str>,
    commit_prefix: Option<&str>,
) -> String {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init");
    run_br(Some(dir), &["init"]).expect("br init");
    std::fs::write(dir.join(".beads/policy.yaml"), policy).unwrap();
    let mut args = vec![
        "create", "--title", title, "--slug", slug, "-p", priority, "-d", "work",
    ];
    if let Some(v) = verify {
        args.push("--verify");
        args.push(v);
        if let Some(p) = principle {
            args.push("--principle");
            args.push(p);
        }
    } else {
        // No VERIFY fence: `br q` models a triage/dupe bead (see verify arg).
        // Never carries --principle, matching the old inline fixture.
        args = vec!["q", title, "-p", priority, "-d", "work"];
    }
    let bead_id = Bead::create(Some(dir), &args).expect("br create");
    if let Some(prefix) = commit_prefix {
        std::fs::write(dir.join("work.txt"), "work\n").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .output()
            .expect("git add");
        assert!(add.status.success());
        let cm = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=test@test",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                &format!("feat: {prefix} ({bead_id})"),
            ])
            .current_dir(dir)
            .output()
            .expect("git commit");
        assert!(
            cm.status.success(),
            "git commit: {}",
            String::from_utf8_lossy(&cm.stderr)
        );
    }
    bead_id
}

/// `br show <id>` → status string (tolerant buddy of `status_of`).
#[cfg(test)]
pub fn bead_status(dir: &std::path::Path, bead_id: &str) -> String {
    let raw = run_br(Some(dir), &["show", bead_id, "--json"]).expect("br show");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("br show json");
    let bead = match &v {
        serde_json::Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
        obj @ serde_json::Value::Object(_) => obj.clone(),
        _ => Value::Null,
    };
    bead.get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_prefers_pinned_binary() {
        let _g = crate::config::lock_env();
        let prev = std::env::var_os("FLYWHEEL_BR_BIN");
        unsafe { std::env::set_var("FLYWHEEL_BR_BIN", "br-pinned") };
        assert_eq!(br_program(), "br-pinned");
        match prev {
            Some(v) => unsafe { std::env::set_var("FLYWHEEL_BR_BIN", v) },
            None => unsafe { std::env::remove_var("FLYWHEEL_BR_BIN") },
        }
        assert_eq!(br_program(), "br");
    }

    #[test]
    fn parse_ready_beads_json_handles_array_shape() {
        let raw = r#"[{"id":"bd-a","title":"one"},{"id":"bd-b","title":"two"}]"#;
        let beads = parse_ready_beads_json(raw).unwrap();
        assert_eq!(beads.len(), 2);
        assert_eq!(beads[0].id, "bd-a");
        assert_eq!(beads[1].title, "two");
    }

    #[test]
    fn parse_ready_beads_json_carries_labels() {
        let raw = r#"[{"id":"bd-a","title":"one","labels":["wave-1","docs"]},{"id":"bd-b","title":"two"}]"#;
        let beads = parse_ready_beads_json(raw).unwrap();
        assert_eq!(beads[0].labels, vec!["wave-1", "docs"]);
        assert!(beads[1].labels.is_empty(), "absent labels -> empty");
    }

    #[test]
    fn parse_ready_beads_json_carries_typed_wave() {
        let raw = r#"[{"id":"bd-w","title":"w","wave":3},{"id":"bd-n","title":"n","wave":null}]"#;
        let beads = parse_ready_beads_json(raw).unwrap();
        assert_eq!(beads[0].wave, Some(3));
        assert_eq!(beads[1].wave, None, "null -> no wave, not 0");
    }

    #[test]
    fn parse_inprogress_beads_json_handles_issues_wrapper() {
        let raw = r#"{"issues":[{"id":"bd-c","title":"three"}],"total":1}"#;
        let beads = parse_inprogress_beads_json(raw).unwrap();
        assert_eq!(beads.len(), 1);
        assert_eq!(beads[0].id, "bd-c");
        let beads = parse_inprogress_beads_json(r#"[{"id":"bd-d","title":"four"}]"#).unwrap();
        assert_eq!(beads[0].id, "bd-d");
    }

    #[test]
    fn parse_inprogress_beads_json_carries_assignee() {
        let raw = r#"{"issues":[
            {"id":"bd-a","title":"one","assignee":"oc"},
            {"id":"bd-b","title":"two","assignee":"captain"},
            {"id":"bd-c","title":"three"},
            {"id":"bd-d","title":"four","assignee":""}
        ]}"#;
        let beads = parse_inprogress_beads_json(raw).unwrap();
        assert_eq!(beads[0].assignee.as_deref(), Some("oc"));
        assert_eq!(beads[1].assignee.as_deref(), Some("captain"));
        assert_eq!(beads[2].assignee, None, "absent assignee -> None");
        assert_eq!(beads[3].assignee, None, "empty assignee -> None");
    }

    #[test]
    fn parse_issue_rows_json_accepts_both_envelopes_and_refuses_unknown() {
        let bare = parse_issue_rows_json(r#"[{"id":"bd-a","status":"open"}]"#, "br list").unwrap();
        assert_eq!(bare.len(), 1);
        let wrapped =
            parse_issue_rows_json(r#"{"issues":[{"id":"bd-b"}],"total":1}"#, "br list").unwrap();
        assert_eq!(wrapped.len(), 1);
        assert!(parse_issue_rows_json(r#"{"rows":[]}"#, "br list").is_err());
        assert!(parse_issue_rows_json("not json", "br list").is_err());
    }

    #[test]
    fn typed_verify_parse_ready_beads_json_populates_verify() {
        let raw = serde_json::to_string(&serde_json::json!([
            {"id":"bd-a","title":"one","verify":"cargo test -p flywheel verify_fence"},
            {"id":"bd-b","title":"two","verify":""}
        ]))
        .unwrap();
        let beads = parse_ready_beads_json(&raw).unwrap();
        assert_eq!(beads.len(), 2);
        assert_eq!(
            beads[0].verify.as_deref(),
            Some("cargo test -p flywheel verify_fence"),
            "typed field -> verify command"
        );
        assert_eq!(beads[1].verify, None, "empty field -> not ready");
    }

    #[test]
    fn parse_ready_beads_json_rejects_garbage() {
        assert!(parse_ready_beads_json("not json").is_err());
    }

    #[test]
    fn br_status_parses_closed_open_unknown() {
        assert_eq!(br_status_of_stdout(r#"{"status":"closed"}"#), "closed");
        assert_eq!(br_status_of_stdout(r#"{"status":"open"}"#), "open");
        assert_eq!(br_status_of_stdout(r#"{"status":"in_progress"}"#), "open");
        assert_eq!(
            br_status_of_stdout(r#"[{"id":"bd-x","status":"closed"}]"#),
            "closed"
        );
        assert_eq!(
            br_status_of_stdout(r#"{"error":{"code":"ISSUE_NOT_FOUND"}}"#),
            "unknown"
        );
        assert_eq!(br_status_of_stdout(""), "unknown");
        assert_eq!(br_status_of_stdout("garbage"), "unknown");
    }
}

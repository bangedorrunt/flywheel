//! Preflight checks — verify prerequisites for a fleet operation before dispatching.
//!
//! Runs before `steer`, `loop run`, and `workflow run --execute` to catch
//! environment/dependency failures in <1s instead of 30s+ into a Resonate
//! cycle. `--skip-preflight` on each command bypasses the auto-check.
//!
//! governed-by: ADR-0005, ADR-0010

use std::path::Path;

use serde_json::{json, Value};

use crate::config::Config;
use crate::herdr_shim::Client;
use crate::mail_shim::Mail;
use crate::{command_exists, resonate_health};

// ─── types ───────────────────────────────────────────────────────────────────

/// One check result.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
}

/// Check outcome.
#[derive(Debug, Clone)]
pub enum Status {
    /// Prerequisite met.
    Pass,
    /// Prerequisite not met — the operation will likely fail if dispatched.
    Fail(String),
    /// Not applicable to this mode (e.g. `.beads/` when not in `--work`).
    Skip(&'static str),
}

impl Check {
    fn pass(name: &'static str) -> Self {
        Self { name, status: Status::Pass }
    }
    fn fail(name: &'static str, msg: impl Into<String>) -> Self {
        Self { name, status: Status::Fail(msg.into()) }
    }
    #[allow(dead_code)]
    fn skip(name: &'static str, reason: &'static str) -> Self {
        Self { name, status: Status::Skip(reason) }
    }
}

/// Which operation we're preflighting — controls which checks run.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mode {
    pub needs_beads: bool,
    pub needs_project: bool,
    pub needs_mail: bool,
}

impl Mode {
    /// Steer: needs herdr, resonate, mail, project dir.
    pub fn steer() -> Self {
        Self { needs_beads: false, needs_project: true, needs_mail: true }
    }
    /// Loop `--work`: needs herdr, resonate, br, beads, mail, project dir.
    pub fn loop_work() -> Self {
        Self { needs_beads: true, needs_project: true, needs_mail: true }
    }
    /// Loop `--queue` (static stages): needs herdr, resonate, mail, project dir.
    pub fn loop_queue() -> Self {
        Self { needs_beads: false, needs_project: true, needs_mail: true }
    }
    /// Workflow `--execute`: needs herdr, resonate, mail, project dir.
    pub fn workflow() -> Self {
        Self { needs_beads: false, needs_project: true, needs_mail: true }
    }
    /// Cron (headless): needs resonate only.
    pub fn cron() -> Self {
        Self { needs_beads: false, needs_project: false, needs_mail: false }
    }
}

// ─── runner ──────────────────────────────────────────────────────────────────

/// Run preflight checks. Returns ALL results (does not short-circuit — an
/// agent sees every failure, not just the first).
pub fn run(cfg: &Config, mode: Mode, project: Option<&str>) -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. herdr socket alive (cheapest meaningful check)
    checks.push(check_herdr(cfg));

    // 2. resonate server reachable
    checks.push(check_resonate(cfg));

    // 3. br binary on PATH
    checks.push(check_br());

    // 4. project directory exists
    if mode.needs_project {
        if let Some(p) = project {
            checks.push(check_project_dir(p));
        } else {
            checks.push(Check::skip("project-dir", "no project specified"));
        }
    }

    // 5. .beads/ exists (--work mode only)
    if mode.needs_beads {
        if let Some(p) = project {
            checks.push(check_beads(p));
        } else {
            checks.push(Check::fail(
                "beads-db",
                "no project specified (required for --work)",
            ));
        }
    }

    // 6. operator key (for toron-privileged ops)
    checks.push(check_operator_key());

    // 7. toron mail health
    if mode.needs_mail {
        checks.push(check_mail(cfg));
    }

    checks
}

/// True if no check failed (Skips are advisory — the operation can proceed).
pub fn all_pass(checks: &[Check]) -> bool {
    checks
        .iter()
        .all(|c| !matches!(c.status, Status::Fail(_)))
}

/// Human-readable summary (stderr-friendly).
pub fn render(checks: &[Check]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "preflight:");
    for c in checks {
        let line = match &c.status {
            Status::Pass => format!("  {} {}", crate::output::ok("PASS"), c.name),
            Status::Fail(msg) => {
                format!("  {} {} — {}", crate::output::error("FAIL"), c.name, msg)
            }
            Status::Skip(reason) => {
                format!("  {} {} ({})", crate::output::dim("SKIP"), c.name, reason)
            }
        };
        let _ = writeln!(out, "{line}");
    }
    let failed = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Fail(_)))
        .count();
    if failed > 0 {
        let _ = writeln!(
            out,
            "  {} check(s) failed — fix or pass --skip-preflight",
            failed
        );
    }
    out
}

/// JSON representation for `--json` output.
pub fn to_json(checks: &[Check]) -> Value {
    json!({
        "passed": all_pass(checks),
        "checks": checks.iter().map(|c| {
            let (status, message) = match &c.status {
                Status::Pass => ("pass", None),
                Status::Fail(msg) => ("fail", Some(msg.as_str())),
                Status::Skip(reason) => ("skip", Some(*reason)),
            };
            json!({
                "name": c.name,
                "status": status,
                "message": message,
            })
        }).collect::<Vec<_>>(),
    })
}

// ─── individual checks (cheapest first) ──────────────────────────────────────

fn check_herdr(cfg: &Config) -> Check {
    match Client::connect(Some(&cfg.socket_path())) {
        Ok(mut c) => match c.ping() {
            Ok(_) => Check::pass("herdr-socket"),
            Err(e) => Check::fail("herdr-socket", format!("ping failed: {e}")),
        },
        Err(e) => Check::fail(
            "herdr-socket",
            format!("connect failed: {e} — is herdr running?"),
        ),
    }
}

fn check_resonate(cfg: &Config) -> Check {
    match resonate_health(cfg) {
        Ok(_) => Check::pass("resonate"),
        Err(e) => Check::fail("resonate", format!("{e}")),
    }
}

fn check_br() -> Check {
    if command_exists("br") {
        Check::pass("br-binary")
    } else {
        Check::fail("br-binary", "br not found on PATH — install beads_rust")
    }
}

fn check_project_dir(project: &str) -> Check {
    if Path::new(project).exists() {
        Check::pass("project-dir")
    } else {
        Check::fail("project-dir", format!("{project} does not exist"))
    }
}

fn check_beads(project: &str) -> Check {
    let beads = Path::new(project).join(".beads");
    if beads.exists() {
        Check::pass("beads-db")
    } else {
        Check::fail(
            "beads-db",
            format!(
                "no .beads/ in {project} — run `br init` or use --queue instead of --work"
            ),
        )
    }
}

fn check_operator_key() -> Check {
    if std::env::var("TORON_OPERATOR_KEY").is_ok() {
        return Check::pass("operator-key");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let default = Path::new(&home).join(".config/toron/operator_nsec");
    if default.exists() {
        Check::pass("operator-key")
    } else {
        Check::skip(
            "operator-key",
            "not configured (needed for ensure_project, force-release)",
        )
    }
}

fn check_mail(cfg: &Config) -> Check {
    match Mail::from_config(Some(&cfg.toron_mail_config())) {
        Ok(Some(m)) => match m.health() {
            Ok(_) => Check::pass("toron-mail"),
            Err(e) => Check::fail("toron-mail", format!("{e}")),
        },
        Ok(None) => Check::skip("toron-mail", "embedded in-process (no daemon)"),
        Err(e) => Check::fail("toron-mail", format!("{e}")),
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_pass_with_passes_and_skips() {
        let checks = vec![
            Check::pass("a"),
            Check::skip("b", "n/a"),
            Check::pass("c"),
        ];
        assert!(all_pass(&checks));
    }

    #[test]
    fn all_pass_false_with_any_fail() {
        let checks = vec![Check::pass("a"), Check::fail("b", "broken")];
        assert!(!all_pass(&checks));
    }

    #[test]
    fn to_json_shapes_status_and_message() {
        let checks = vec![
            Check::pass("herdr-socket"),
            Check::fail("resonate", "unreachable"),
            Check::skip("beads-db", "not work mode"),
        ];
        let v = to_json(&checks);
        assert_eq!(v["passed"], false);
        assert_eq!(v["checks"][0]["status"], "pass");
        assert_eq!(v["checks"][1]["status"], "fail");
        assert_eq!(v["checks"][1]["message"], "unreachable");
        assert_eq!(v["checks"][2]["status"], "skip");
    }

    #[test]
    fn render_includes_failures_and_count() {
        let checks = vec![
            Check::pass("herdr-socket"),
            Check::fail("br-binary", "br not found on PATH"),
        ];
        let out = render(&checks);
        assert!(out.contains("br-binary"));
        assert!(out.contains("1 check(s) failed"));
    }

    #[test]
    fn mode_loop_work_needs_beads_and_project() {
        let m = Mode::loop_work();
        assert!(m.needs_beads);
        assert!(m.needs_project);
        assert!(m.needs_mail);
    }

    #[test]
    fn mode_steer_skips_beads() {
        let m = Mode::steer();
        assert!(!m.needs_beads);
        assert!(m.needs_project);
    }

    #[test]
    fn mode_cron_is_minimal() {
        let m = Mode::cron();
        assert!(!m.needs_beads);
        assert!(!m.needs_project);
        assert!(!m.needs_mail);
    }

    #[test]
    fn render_all_pass_no_failure_line() {
        let checks = vec![Check::pass("a"), Check::skip("b", "n/a")];
        let out = render(&checks);
        assert!(!out.contains("failed"));
    }
}

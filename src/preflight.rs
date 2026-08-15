//! Preflight checks — verify prerequisites for a fleet operation before dispatching.
//!
//! Runs before `steer`, `loop run`, and `workflow run --execute` to catch
//! environment/dependency failures in <1s instead of 30s+ into a Resonate
//! cycle. `--skip-preflight` on each command bypasses the auto-check.
//!
//! governed-by: ADR-0005, ADR-0010

use std::path::Path;

use serde_json::{Value, json};

use crate::config::Config;
use crate::herdr_shim::{Client, WorktreeInfo};
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
        Self {
            name,
            status: Status::Pass,
        }
    }
    fn fail(name: &'static str, msg: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail(msg.into()),
        }
    }
    #[allow(dead_code)]
    fn skip(name: &'static str, reason: &'static str) -> Self {
        Self {
            name,
            status: Status::Skip(reason),
        }
    }
}

/// Which operation we're preflighting — controls which checks run.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mode {
    pub needs_beads: bool,
    pub needs_project: bool,
    pub needs_mail: bool,
    /// ADR-0019 Spec 3.3: `loop run` / `workflow run --execute` fail when
    /// the project workspace carries an extra linked worktree — the loop
    /// never adopts a second tree.
    pub needs_worktree: bool,
    /// Operator opt-out (`--allow-worktree`, operator-only + logged): skip
    /// the linked-worktree check for a deliberate human experiment.
    pub allow_worktree: bool,
    /// ADR-0019 Spec 2: the dispatched agent's standing orders exist —
    /// global (~/.config/flywheel/standing-orders.md, generated on first
    /// run) or the project fence (<project>/.flywheel/standing-orders.md).
    pub needs_standing_orders: bool,
    /// ADR-0019 Spec 6 (`subtract-before-you-add` structural encoding): a
    /// playbook run must not create *V2* / *_improved* / *_enhanced* files.
    pub needs_playbook_files: bool,
}

impl Mode {
    /// Steer: needs herdr, resonate, mail, project dir.
    pub fn steer() -> Self {
        Self {
            needs_beads: false,
            needs_project: true,
            needs_mail: true,
            needs_worktree: false,
            allow_worktree: false,
            needs_standing_orders: true,
            needs_playbook_files: false,
        }
    }
    /// Loop `--work`: needs herdr, resonate, br, beads, mail, project dir.
    pub fn loop_work() -> Self {
        Self {
            needs_beads: true,
            needs_project: true,
            needs_mail: true,
            needs_worktree: true,
            allow_worktree: false,
            needs_standing_orders: true,
            needs_playbook_files: false,
        }
    }
    /// Loop `--queue` (static stages): needs herdr, resonate, mail, project dir.
    pub fn loop_queue() -> Self {
        Self {
            needs_beads: false,
            needs_project: true,
            needs_mail: true,
            needs_worktree: true,
            allow_worktree: false,
            needs_standing_orders: true,
            needs_playbook_files: false,
        }
    }
    /// Workflow `--execute`: needs herdr, resonate, mail, project dir, and
    /// the playbook-run file conventions (no *V2* / *_improved* /
    /// *_enhanced* new files).
    pub fn workflow() -> Self {
        Self {
            needs_beads: false,
            needs_project: true,
            needs_mail: true,
            needs_worktree: true,
            allow_worktree: false,
            needs_standing_orders: true,
            needs_playbook_files: true,
        }
    }
    /// Cron (headless): needs resonate only.
    pub fn cron() -> Self {
        Self {
            needs_beads: false,
            needs_project: false,
            needs_mail: false,
            needs_worktree: false,
            allow_worktree: false,
            needs_standing_orders: false,
            needs_playbook_files: false,
        }
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

    // 8. ADR-0019 Spec 3.3: no extra linked worktree in the project
    //    workspace (loop run / workflow run --execute). `--allow-worktree`
    //    is the operator-only opt-out (refused without an operator key,
    //    logged at the call site).
    if mode.needs_worktree {
        if mode.allow_worktree {
            if worktree_optout_legal(mode.allow_worktree, operator_key_configured()) {
                checks.push(Check::skip(
                    "worktree",
                    "--allow-worktree operator opt-out (ADR-0019 Spec 3.3, logged)",
                ));
            } else {
                checks.push(Check::fail(
                    "worktree",
                    "--allow-worktree is operator-only (ADR-0019 Spec 3.3): no operator key configured",
                ));
            }
        } else if let Some(p) = project {
            checks.push(check_worktree(cfg, p));
        } else {
            checks.push(Check::fail(
                "worktree",
                "no project specified (required for the worktree check)",
            ));
        }
    }

    // 9. ADR-0019 Spec 2: standing orders exist for the dispatched agent —
    //    global (generated on first run) or the project fence.
    if mode.needs_standing_orders {
        checks.push(check_standing_orders(cfg, project));
    }

    // 10. ADR-0019 Spec 6 (`subtract-before-you-add`): a playbook run must
    //     not create *V2* / *_improved* / *_enhanced* new files (the repo
    //     no-proliferation convention, now enforced at preflight).
    if mode.needs_playbook_files {
        if let Some(p) = project {
            checks.push(check_playbook_files(cfg, p));
        } else {
            checks.push(Check::fail(
                "playbook-files",
                "no project specified (required for the new-file scan)",
            ));
        }
    }

    checks
}

/// True if no check failed (Skips are advisory — the operation can proceed).
pub fn all_pass(checks: &[Check]) -> bool {
    checks.iter().all(|c| !matches!(c.status, Status::Fail(_)))
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
            format!("no .beads/ in {project} — run `br init` or use --queue instead of --work"),
        )
    }
}

fn check_operator_key() -> Check {
    if operator_key_configured() {
        Check::pass("operator-key")
    } else {
        Check::skip(
            "operator-key",
            "not configured (needed for ensure_project, force-release)",
        )
    }
}

/// Operator key configured? The gate for operator-only opt-outs
/// (`--allow-worktree`, ADR-0019 Spec 3.3). Same sources as
/// [`check_operator_key`]: `TORON_OPERATOR_KEY` or
/// `~/.config/toron/operator_nsec`.
pub fn operator_key_configured() -> bool {
    if std::env::var("TORON_OPERATOR_KEY").is_ok() {
        return true;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home)
        .join(".config/toron/operator_nsec")
        .exists()
}

/// Pure: is the `--allow-worktree` opt-out legal? It is operator-only —
/// refused when no operator key is configured (ADR-0019 Spec 3.3).
pub fn worktree_optout_legal(allow_worktree: bool, operator_key: bool) -> bool {
    !allow_worktree || operator_key
}

/// Pure: the linked-worktree subset of a `worktree.list` — the workspace's
/// own (primary) tree is expected and fine; only LINKED trees are the extra
/// checkouts the loop must never adopt.
pub fn linked_worktrees(v: &[WorktreeInfo]) -> Vec<&WorktreeInfo> {
    v.iter().filter(|w| w.is_linked_worktree).collect()
}

/// ADR-0019 Spec 3.3: fail the run when the project workspace carries an
/// extra linked worktree — the loop will not dispatch `--work` into it and
/// will not ledger-check a SHA from it. Removing it (or the operator's
/// `--allow-worktree`) unblocks the run.
fn check_worktree(cfg: &Config, project: &str) -> Check {
    let mut c = match Client::connect(Some(&cfg.socket_path())) {
        Ok(c) => c,
        Err(e) => return Check::fail("worktree", format!("herdr connect failed: {e}")),
    };
    let ws_id = match crate::find_workspace(&mut c, project) {
        Ok(id) => id,
        Err(e) => return Check::fail("worktree", format!("workspace lookup failed: {e}")),
    };
    let all = match crate::herdr_shim::worktree_list(&mut c, Some(&ws_id)) {
        Ok(v) => v,
        Err(e) => return Check::fail("worktree", format!("worktree.list failed: {e}")),
    };
    let linked = linked_worktrees(&all);
    if linked.is_empty() {
        return Check::pass("worktree");
    }
    let paths: Vec<String> = linked.iter().map(|w| w.path.clone()).collect();
    Check::fail(
        "worktree",
        format!(
            "{} linked worktree(s): {} — worktrees are forbidden in the loop (ADR-0019 Spec 3); remove them or pass --allow-worktree (operator-only, logged)",
            linked.len(),
            paths.join(", ")
        ),
    )
}

/// Pure: does a filename violate the `subtract-before-you-add`
/// no-proliferation convention (ADR-0019 Spec 6 / repo AGENTS.md)? Matches
/// `*V2*` (case-insensitive), the version-suffix family `v[0-9]+$`
/// (foo_v3.rs / fooV4.rs), `*_improved*`, and `*_enhanced*`.
pub fn is_forbidden_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains("_improved") || lower.contains("_enhanced") {
        return true;
    }
    if lower.contains("v2") {
        return true;
    }
    // version suffix `v<digits>` at the end of the stem (foo_v3, fooV4)
    if let Some(stem) = Path::new(name).file_stem() {
        let s = stem.to_string_lossy().to_ascii_lowercase();
        let stripped = s.trim_end_matches(|c: char| c.is_ascii_digit());
        if stripped.len() < s.len() && stripped.ends_with('v') {
            return true;
        }
    }
    false
}

/// Scan a git working tree for NEW files (untracked `??` or staged adds
/// `A `) whose names violate the no-proliferation convention. Pure-ish —
/// runs `git status --porcelain` in `dir`; parse + filter are testable.
pub fn scan_new_forbidden_files(dir: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // porcelain: `XY path` — X = index status, Y = worktree status.
            // New files are `??` (untracked) or index `A` (staged add).
            let code = line.get(..2).unwrap_or("");
            let is_new = code == "??" || code.starts_with('A');
            if !is_new {
                return None;
            }
            let path = line.get(3..).unwrap_or("").trim();
            if path.is_empty() || path.starts_with('"') {
                return None; // quoted paths (spaces) — out of scope for the scan
            }
            is_forbidden_filename(path).then(|| path.to_string())
        })
        .collect()
}

/// ADR-0019 Spec 2: the standing orders exist for a dispatched agent —
/// the GLOBAL generated file (~/.config/flywheel/standing-orders.md) or the
/// PROJECT fence (<project>/.flywheel/standing-orders.md). Missing both ->
/// generate the global on first run; a generation failure fails the check
/// with the regen command in the message.
pub fn check_standing_orders(cfg: &Config, project: Option<&str>) -> Check {
    let global = crate::config::config_dir().join("standing-orders.md");
    let fence = project.map(|p| {
        crate::project_dir(cfg, p)
            .join(".flywheel")
            .join("standing-orders.md")
    });
    check_standing_orders_at(&global, fence.as_deref())
}

/// The standing-orders check with explicit paths (testable without touching
/// the real config dir).
pub fn check_standing_orders_at(global: &Path, fence: Option<&Path>) -> Check {
    if global.exists() {
        return Check::pass("standing-orders");
    }
    if fence.is_some_and(|f| f.exists()) {
        return Check::pass("standing-orders");
    }
    // first run: generate the global, then require it present
    match crate::standing_orders::regen_global(global) {
        Ok(_) => Check::pass("standing-orders"),
        Err(e) => Check::fail(
            "standing-orders",
            format!(
                "standing orders missing and generation failed at {}: {e}; run `flywheel standing-orders regen`",
                global.display()
            ),
        ),
    }
}

/// ADR-0019 Spec 6: a playbook run must not create *V2* / *_improved* /
/// *_enhanced* new files — scan the project tree's untracked/staged adds
/// and fail naming each offender + the principle.
fn check_playbook_files(cfg: &Config, project: &str) -> Check {
    let dir = crate::project_dir(cfg, project);
    let bad = scan_new_forbidden_files(&dir);
    if bad.is_empty() {
        Check::pass("playbook-files")
    } else {
        Check::fail(
            "playbook-files",
            format!(
                "new file(s) violate `subtract-before-you-add` (ADR-0019 Spec 6): {} — no *V2* / *_improved* / *_enhanced* files",
                bad.join(", ")
            ),
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
        let checks = vec![Check::pass("a"), Check::skip("b", "n/a"), Check::pass("c")];
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

    /// ADR-0019 Spec 3.3: `loop run` and `workflow run --execute` preflight
    /// the workspace for extra linked worktrees (forbidden in the loop);
    /// steer and cron do not. `--allow-worktree` is the operator opt-out.
    #[test]
    fn mode_dispatch_modes_check_worktree_steer_cron_do_not() {
        for m in [Mode::loop_work(), Mode::loop_queue(), Mode::workflow()] {
            assert!(m.needs_worktree, "dispatch mode checks worktrees");
            assert!(!m.allow_worktree, "opt-out is never the default");
        }
        for m in [Mode::steer(), Mode::cron()] {
            assert!(!m.needs_worktree, "steer/cron do not check worktrees");
        }
    }

    /// ADR-0019 Spec 3.3: only LINKED worktrees are extra — the workspace's
    /// own (primary) tree is expected.
    #[test]
    fn linked_worktrees_filters_primary_tree() {
        let primary = WorktreeInfo {
            path: "/w/main".into(),
            branch: Some("main".into()),
            is_bare: false,
            is_detached: false,
            is_prunable: false,
            is_linked_worktree: false,
            open_workspace_id: Some("ws-main".into()),
            label: String::new(),
        };
        let linked = WorktreeInfo {
            path: "/w/exp".into(),
            branch: Some("exp".into()),
            is_bare: false,
            is_detached: true,
            is_prunable: true,
            is_linked_worktree: true,
            open_workspace_id: None,
            label: "exp".into(),
        };
        let v = vec![primary, linked];
        let linked_only = linked_worktrees(&v);
        assert_eq!(linked_only.len(), 1);
        assert_eq!(linked_only[0].path, "/w/exp");
        assert!(linked_worktrees(&[]).is_empty());
    }

    /// ADR-0019 Spec 3.3: `--allow-worktree` is operator-only — the opt-out
    /// is refused when no operator key is configured.
    #[test]
    fn allow_worktree_requires_operator_key() {
        assert!(
            !worktree_optout_legal(true, false),
            "no operator key -> --allow-worktree refused"
        );
        assert!(
            worktree_optout_legal(true, true),
            "operator key configured -> opt-out legal"
        );
        assert!(
            worktree_optout_legal(false, false),
            "no opt-out, no key -> fine"
        );
        assert!(worktree_optout_legal(false, true));
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

    // ---- bd-w3-preflight-m15j: standing orders + playbook file scan ----

    /// The no-proliferation patterns: *V2* (case-insensitive), version
    /// suffix v[0-9]+, *_improved*, *_enhanced*.
    #[test]
    fn forbidden_filename_rejects_v2_improved_enhanced() {
        for bad in [
            "fooV2.rs",
            "document_processorV2.rs",
            "foo_v2.rs",
            "FOO_V2.RS",
            "foo_v3.rs",
            "fooV4.rs",
            "doc_improved.rs",
            "doc_enhanced.rs",
            "parser_improved_v2.rs",
        ] {
            assert!(is_forbidden_filename(bad), "{bad} must be rejected");
        }
        // negative cases — normal new files pass
        for good in [
            "main.rs",
            "auth.rs",
            "parser.rs",
            "utils.rs",
            "docs/design.md",
            "review.rs",
            "service2.rs",
            "csv.rs",
            "v.rs",
            "av.rs",
        ] {
            assert!(!is_forbidden_filename(good), "{good} must pass");
        }
    }

    /// Missing global + missing fence -> the check GENERATES the global on
    /// first run (pass + file present).
    #[test]
    fn standing_orders_check_generates_when_missing() {
        let dir = std::env::temp_dir().join(format!("so-generate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let global = dir.join("config").join("standing-orders.md");
        let check = check_standing_orders_at(&global, None);
        assert!(
            matches!(check.status, Status::Pass),
            "generated on first run: {check:?}"
        );
        assert!(global.exists(), "global written");
        assert!(
            std::fs::read_to_string(&global)
                .unwrap()
                .contains("claim before work")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Generation failure (unwritable parent) -> Fail with the regen command
    /// in the message (acceptance: missing standing orders is actionable).
    #[test]
    fn standing_orders_check_fails_with_actionable_message_when_unwritable() {
        let dir = std::env::temp_dir().join(format!("so-unwritable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // a FILE where the config dir should be -> create_dir_all/write fails
        let blocker = dir.join("config");
        std::fs::write(&blocker, "not a dir").unwrap();
        let global = blocker.join("standing-orders.md");
        let check = check_standing_orders_at(&global, None);
        match check.status {
            Status::Fail(msg) => {
                assert!(msg.contains("standing-orders regen"), "actionable: {msg}");
                assert!(
                    msg.contains("standing orders missing"),
                    "names the gap: {msg}"
                );
            }
            other => panic!("expected Fail, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A project fence satisfies the check even when the global is missing.
    #[test]
    fn standing_orders_check_passes_with_project_fence() {
        let dir = std::env::temp_dir().join(format!("so-fence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fence_dir = dir.join("proj").join(".flywheel");
        std::fs::create_dir_all(&fence_dir).unwrap();
        let fence = fence_dir.join("standing-orders.md");
        std::fs::write(&fence, "# authored fence\n").unwrap();
        let global = dir.join("config").join("standing-orders.md");
        let check = check_standing_orders_at(&global, Some(&fence));
        assert!(
            matches!(check.status, Status::Pass),
            "fence satisfies: {check:?}"
        );
        assert!(
            !global.exists(),
            "global not generated when the fence suffices"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Present global -> pass.
    #[test]
    fn standing_orders_check_passes_when_global_present() {
        let dir = std::env::temp_dir().join(format!("so-present-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let global = dir.join("standing-orders.md");
        std::fs::write(&global, "# present\n").unwrap();
        let check = check_standing_orders_at(&global, None);
        assert!(matches!(check.status, Status::Pass), "{check:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// THE fixture: a playbook run creating fooV2.rs fails preflight; a
    /// normal new file passes (acceptance).
    #[test]
    fn playbook_scan_rejects_v2_new_file_and_passes_normal() {
        let dir = std::env::temp_dir().join(format!("pf-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init");
        // normal new file -> passes
        std::fs::write(dir.join("auth.rs"), "// ok\n").unwrap();
        assert!(
            scan_new_forbidden_files(&dir).is_empty(),
            "normal new file passes the scan"
        );
        // fooV2.rs -> fails, naming the file
        std::fs::write(dir.join("fooV2.rs"), "// v2\n").unwrap();
        let bad = scan_new_forbidden_files(&dir);
        assert_eq!(bad, vec!["fooV2.rs".to_string()], "rejects the V2 file");
        // staged add also flagged (git add)
        let add = std::process::Command::new("git")
            .args(["add", "fooV2.rs"])
            .current_dir(&dir)
            .output()
            .expect("git add");
        assert!(add.status.success());
        let bad = scan_new_forbidden_files(&dir);
        assert!(
            bad.iter().any(|f| f == "fooV2.rs"),
            "staged add still flagged: {bad:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

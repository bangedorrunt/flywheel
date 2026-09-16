// governed-by: ADR-0026
//! Preflight checks — verify prerequisites for a fleet operation before dispatching.
//!
//! Runs before `steer`, `loop run`, and `workflow run --execute` to catch
//! environment/dependency failures in <1s instead of 30s+ into a Resonate
//! cycle. `--skip-preflight` on each command bypasses the auto-check.
//!
//! governed-by: ADR-0005, ADR-0010, ADR-0026

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

    // 1. herdr socket alive (cheapest meaningful check) — skipped for modes
    //    that never touch panes (cron is store-side only).
    if mode.needs_project || mode.needs_beads || mode.needs_mail {
        checks.push(check_herdr(cfg));
    }

    // 2. resonate server reachable
    checks.push(check_resonate(cfg));

    // 2b. doorbell daemon (bd-0gmw): advisory only — a configured daemon
    //     being down degrades to the embedded loop, so this warns (skip)
    //     and never blocks.
    checks.push(check_doorbell(cfg));

    // 3. br binary on PATH — only bead-driven modes (matches the Mode doc:
    //    "Cron (headless): needs resonate only")
    if mode.needs_beads {
        checks.push(check_br());
    }

    // 4. project directory exists
    if mode.needs_project {
        if let Some(p) = project {
            checks.push(check_project_dir(cfg, p));
        } else {
            checks.push(Check::skip("project-dir", "no project specified"));
        }
    }

    // 5. .beads/ exists (--work mode only)
    if mode.needs_beads {
        if let Some(p) = project {
            checks.push(check_beads(cfg, p));
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

    checks.extend(provider_funded_checks(cfg, project));
    if mode.needs_beads || mode.needs_worktree {
        checks.push(check_loop_identity(cfg, project));
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
        Ok(()) => Check::pass("resonate"),
        Err(e) => Check::fail("resonate", format!("{e}")),
    }
}

/// bd-0gmw: advisory doorbell entry. A configured daemon being down is a
/// warning (skip), never a FAIL — embedded mode still works. Mirrors the
/// advisory-skip pattern; there is no blocking outcome here.
fn check_doorbell(cfg: &Config) -> Check {
    match cfg.doorbell_url() {
        None => Check::skip("doorbell", "embedded (no [doorbell] url configured)"),
        Some(url) => {
            if crate::doorbell::daemon_healthy(&url) {
                Check::pass("doorbell")
            } else {
                Check::skip(
                    "doorbell",
                    "configured daemon unreachable — remediation: doorbell start | doorbell install | flywheel daemon start",
                )
            }
        }
    }
}

fn check_br() -> Check {
    if command_exists("br") {
        Check::pass("br-binary")
    } else {
        Check::fail("br-binary", "br not found on PATH — install beads_rust")
    }
}

fn check_project_dir(cfg: &Config, project: &str) -> Check {
    // bd-adr22-loop-preflight: resolve through project_dir (slug keys join
    // the workspace root; absolute paths pass through). The literal
    // Path::new(project) form broke BOTH spellings: a slug arg only passed
    // when cwd happened to contain it, and an absolute-path arg then failed
    // the slug-keyed swarm lookup (worktree check) — the two checks
    // demanded different keys.
    let dir = crate::project_dir(cfg, project);
    if dir.exists() {
        Check::pass("project-dir")
    } else {
        Check::fail("project-dir", format!("{project} does not exist"))
    }
}

fn check_beads(cfg: &Config, project: &str) -> Check {
    let beads = crate::project_dir(cfg, project).join(".beads");
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
    if fence.is_some_and(std::path::Path::exists) {
        return Check::pass("standing-orders");
    }
    // first run: generate the global, then require it present
    match crate::standing_orders::regen_global(global) {
        Ok(()) => Check::pass("standing-orders"),
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
/// True iff `name` is set to a non-empty trimmed value (never logs the value).
fn env_nonempty_trimmed(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

pub fn check_provider_funded(provider: &str, profile: &str) -> Check {
    // GS-037 P0-4: louder provider preflight for opencode-go — name the env
    // vars (never values). Fail-closed when neither key has a non-empty
    // trimmed value; empty/whitespace exports must NOT invent commercial PASS.
    if provider.eq_ignore_ascii_case("opencode-go") || provider.eq_ignore_ascii_case("opencode") {
        // GS-040 PF-EMPTY-KEY-PASS: var_os().is_some() is insufficient —
        // `export OPENCODE_API_KEY=` must not Pass.
        let api = env_nonempty_trimmed("OPENCODE_API_KEY");
        let go = env_nonempty_trimmed("OPENCODE_GO_API_KEY");
        if !api && !go {
            // GS-039 P2: refuse fail-open commercially — missing/empty keys
            // are a hard fail (not SKIP theater). Export OPENCODE_GO_API_KEY
            // (primary) or OPENCODE_API_KEY before funded spawn.
            return Check::fail(
                "provider-funded",
                "opencode-go/opencode: neither OPENCODE_API_KEY nor OPENCODE_GO_API_KEY is set — refuse fail-open (GS-039 P2). Export one (flywheel aliases GO→API into pane env).",
            );
        }
        // GS-040 PF-GO-ONLY-SKIP: documented primary path is OPENCODE_GO_API_KEY
        // alone (alias GO→API into pane env at spawn). GO-only with a non-empty
        // trimmed key is commercial PASS — not Skip theater. API-only / both
        // also PASS.
        return Check::pass("provider-funded");
    }
    let env_key = format!("FLYWHEEL_FUNDED_BALANCE_{}", provider.to_ascii_uppercase());
    if let Ok(val) = std::env::var(&env_key) {
        let lower = val.to_ascii_lowercase();
        if lower == "0" || lower == "0.00" || lower.contains("insufficient") || lower == "empty" {
            return Check::fail(
                "provider-funded",
                format!(
                    "provider '{provider}' (profile '{profile}') funded check failed: balance {val}"
                ),
            );
        }
        if lower == "ok" || lower.contains("sufficient") {
            return Check::pass("provider-funded");
        }
    }
    Check::skip(
        "provider-funded",
        "no funded endpoint — classifier+fallback cover it",
    )
}
fn provider_funded_checks(_cfg: &Config, project: Option<&str>) -> Vec<Check> {
    let Some(_proj) = project else {
        return Vec::new();
    };
    let catalog = match crate::profile::ProfileCatalog::load() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut providers: Vec<(String, String)> = Vec::new();
    // GS-040 PF-SLOT-RESOLVE-OK-SWALLOW: load routing once. If any profile
    // needs @slot resolve and routing is corrupt, fail-loud — never .ok()-
    // swallow into silent "no funded endpoint" SKIP (fail-open invent-green).
    let needs_slot = catalog
        .profiles
        .values()
        .any(|p| p.provider.trim().is_empty() && p.model.starts_with('@'));
    let routing = match crate::models::ModelRegistry::load_routing() {
        Ok(reg) => Some(reg),
        Err(e) if needs_slot => {
            return vec![Check::fail(
                "provider-funded",
                format!(
                    "model routing load failed while resolving @slot profiles — refuse silent SKIP (GS-040): {e}"
                ),
            )];
        }
        Err(_) => None,
    };
    for (name, profile) in &catalog.profiles {
        // GS-040: default profiles pin provider in the model id
        // (`opencode-go/muse-…`) while `profile.provider` stays empty
        // (ADR-0007). Derive provider from model prefix when unset so
        // funded-key checks actually run instead of SKIP theater.
        let mut prov = profile.provider.trim().to_string();
        if prov.is_empty() {
            // Resolve @slot aliases (coder → @standard → opencode-go/muse-…)
            // then take the provider/ half of the composite model id.
            let model_id = if profile.model.starts_with('@') {
                match routing
                    .as_ref()
                    .and_then(|reg| reg.slot(&profile.model).map(|s| s.model.clone()))
                {
                    Some(m) => m,
                    None => {
                        // GS-040: @slot present but unresolved — fail-loud,
                        // never silent SKIP "no funded endpoint".
                        return vec![Check::fail(
                            "provider-funded",
                            format!(
                                "profile '{name}' model '{}' could not resolve @slot — refuse silent SKIP (GS-040)",
                                profile.model
                            ),
                        )];
                    }
                }
            } else {
                profile.model.clone()
            };
            if let Some((p, _)) = model_id.split_once('/') {
                prov = p.trim().to_string();
            }
        }
        if prov.is_empty() {
            continue;
        }
        if seen.insert(prov.clone()) {
            providers.push((prov, name.clone()));
        }
    }
    if providers.is_empty() {
        return vec![Check::skip(
            "provider-funded",
            "no funded endpoint — classifier+fallback cover it",
        )];
    }
    providers
        .into_iter()
        .map(|(prov, prof)| check_provider_funded(&prov, &prof))
        .collect()
}
pub fn check_loop_identity(_cfg: &Config, project: Option<&str>) -> Check {
    if crate::preflight::operator_key_configured() {
        Check::pass("loop-identity")
    } else {
        let proj = project.unwrap_or("<project>");
        Check::fail(
            "loop-identity",
            format!(
                "loop identity for '{proj}' has no local key — register or bind it before dispatching; renewals would soft-fail forever (ADR-0026 Spec 4)"
            ),
        )
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

    #[test]
    fn provider_funded_no_endpoint_is_skip_with_honest_reason() {
        // Serialize env + clear balance override so a parallel zero-balance
        // test (or ambient FLYWHEEL_FUNDED_BALANCE_ZAI) cannot invent Fail.
        let _g = crate::config::lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_FUNDED_BALANCE_ZAI") };
        let c = check_provider_funded("zai", "coder");
        match c.status {
            Status::Skip(r) => assert!(r.contains("no funded endpoint")),
            other => panic!("expected Skip(no funded endpoint), got {other:?}"),
        }
    }

    #[test]
    fn gs040_opencode_keys_present_passes() {
        // Behavioral: non-empty API key → Pass (never print key values).
        let _g = crate::config::lock_env();
        let prev_api = std::env::var_os("OPENCODE_API_KEY");
        let prev_go = std::env::var_os("OPENCODE_GO_API_KEY");
        unsafe {
            std::env::remove_var("OPENCODE_GO_API_KEY");
            std::env::set_var("OPENCODE_API_KEY", "test-api-key-not-a-secret-fixture");
        }
        let c = check_provider_funded("opencode-go", "coder");
        assert!(
            matches!(c.status, Status::Pass),
            "non-empty OPENCODE_API_KEY must PASS provider-funded, got {:?}",
            c.status
        );
        unsafe {
            match prev_api {
                Some(v) => std::env::set_var("OPENCODE_API_KEY", v),
                None => std::env::remove_var("OPENCODE_API_KEY"),
            }
            match prev_go {
                Some(v) => std::env::set_var("OPENCODE_GO_API_KEY", v),
                None => std::env::remove_var("OPENCODE_GO_API_KEY"),
            }
        }
    }

    #[test]
    fn gs040_empty_opencode_api_key_must_not_pass() {
        // PF-EMPTY-KEY-PASS: empty / whitespace export must NOT invent Pass.
        let _g = crate::config::lock_env();
        let prev_api = std::env::var_os("OPENCODE_API_KEY");
        let prev_go = std::env::var_os("OPENCODE_GO_API_KEY");
        for empty in ["", "   ", "\t"] {
            unsafe {
                std::env::remove_var("OPENCODE_GO_API_KEY");
                std::env::set_var("OPENCODE_API_KEY", empty);
            }
            let c = check_provider_funded("opencode-go", "coder");
            assert!(
                !matches!(c.status, Status::Pass),
                "empty/whitespace OPENCODE_API_KEY must NOT Pass provider-funded, got {:?}",
                c.status
            );
            assert!(
                matches!(c.status, Status::Fail(_)),
                "empty/whitespace OPENCODE_API_KEY must Fail (or not Pass), got {:?}",
                c.status
            );
        }
        unsafe {
            match prev_api {
                Some(v) => std::env::set_var("OPENCODE_API_KEY", v),
                None => std::env::remove_var("OPENCODE_API_KEY"),
            }
            match prev_go {
                Some(v) => std::env::set_var("OPENCODE_GO_API_KEY", v),
                None => std::env::remove_var("OPENCODE_GO_API_KEY"),
            }
        }
    }

    #[test]
    fn gs040_go_only_nonempty_must_pass() {
        // PF-GO-ONLY-SKIP: OPENCODE_GO_API_KEY alone (primary path) → Pass.
        let _g = crate::config::lock_env();
        let prev_api = std::env::var_os("OPENCODE_API_KEY");
        let prev_go = std::env::var_os("OPENCODE_GO_API_KEY");
        unsafe {
            std::env::remove_var("OPENCODE_API_KEY");
            std::env::set_var("OPENCODE_GO_API_KEY", "test-go-key-not-a-secret-fixture");
        }
        let c = check_provider_funded("opencode-go", "coder");
        assert!(
            matches!(c.status, Status::Pass),
            "GO-only non-empty must PASS provider-funded, got {:?}",
            c.status
        );
        unsafe {
            match prev_api {
                Some(v) => std::env::set_var("OPENCODE_API_KEY", v),
                None => std::env::remove_var("OPENCODE_API_KEY"),
            }
            match prev_go {
                Some(v) => std::env::set_var("OPENCODE_GO_API_KEY", v),
                None => std::env::remove_var("OPENCODE_GO_API_KEY"),
            }
        }
    }

    #[test]
    fn gs040_slot_resolve_does_not_ok_swallow_routing() {
        // PF-SLOT-RESOLVE-OK-SWALLOW: no load_routing().ok() swallow; fail-loud
        // when @slot cannot resolve.
        let src = include_str!("preflight.rs");
        let start = src
            .find("fn provider_funded_checks")
            .expect("provider_funded_checks");
        let body = &src[start..start + 2500];
        assert!(
            !body.contains("load_routing().ok()")
                && !body.contains("load_routing()\n                    .ok()"),
            "provider_funded_checks must not .ok()-swallow load_routing Err"
        );
        // Also reject the chained form used before the fix.
        assert!(
            !body.contains(
                "load_routing()\n                    .ok()\n                    .and_then"
            ),
            "no chained load_routing().ok().and_then swallow"
        );
        assert!(
            body.contains("refuse silent SKIP (GS-040)") && body.contains("needs_slot"),
            "provider_funded_checks must fail-loud when @slot cannot resolve"
        );
    }

    #[test]
    fn gs039_opencode_go_missing_keys_fails_closed() {
        let _g = crate::config::lock_env();
        let prev_api = std::env::var_os("OPENCODE_API_KEY");
        let prev_go = std::env::var_os("OPENCODE_GO_API_KEY");
        unsafe {
            std::env::remove_var("OPENCODE_API_KEY");
            std::env::remove_var("OPENCODE_GO_API_KEY");
        }
        let c = check_provider_funded("opencode-go", "coder");
        assert!(
            matches!(c.status, Status::Fail(_)),
            "opencode-go missing keys must fail-closed, got {:?}",
            c.status
        );
        let src = include_str!("preflight.rs");
        assert!(
            src.contains("refuse fail-open (GS-039 P2)"),
            "opencode-go missing keys must fail-closed, not SKIP"
        );
        unsafe {
            match prev_api {
                Some(v) => std::env::set_var("OPENCODE_API_KEY", v),
                None => std::env::remove_var("OPENCODE_API_KEY"),
            }
            match prev_go {
                Some(v) => std::env::set_var("OPENCODE_GO_API_KEY", v),
                None => std::env::remove_var("OPENCODE_GO_API_KEY"),
            }
        }
    }

    #[test]
    fn provider_funded_zero_balance_fails_naming_provider() {
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("FLYWHEEL_FUNDED_BALANCE_ZAI", "0") };
        let c = check_provider_funded("zai", "oracle");
        match c.status {
            Status::Fail(m) => {
                assert!(m.contains("zai"));
                assert!(m.contains("oracle"));
            }
            _ => panic!(),
        }
        unsafe { std::env::remove_var("FLYWHEEL_FUNDED_BALANCE_ZAI") };
    }

    #[test]
    fn loop_identity_fails_without_key() {
        let cfg = crate::config::Config::default();
        let _g = crate::config::lock_env();
        unsafe { std::env::remove_var("TORON_OPERATOR_KEY") };
        let home = std::env::temp_dir().join(format!("loop-id-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        unsafe { std::env::set_var("HOME", &home) };
        let c = check_loop_identity(&cfg, Some("demo"));
        assert!(matches!(c.status, Status::Fail(_)));
        unsafe { std::env::remove_var("HOME") };
        let _ = std::fs::remove_dir_all(&home);
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

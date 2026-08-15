// governed-by: ADR-0019
// Wave-1 verdict spine (ADR-0019 Spec 1, amended 2026-08-15): the pure types
// and the legal-close matrix. No IO in this module by design (bd-w1-types-vbez).
// The br-gate verdict store, receipts helper, and loop-only reporter guard
// land in bd-w1-gate-store-h2ur on top of these types.
//
// The pub items here are the wave-1 spine: consumers (br gate wrappers, the
// `flywheel ledger` CLI, the loop close-detect path) land in the next beads.
// Until then nothing outside this module references them, so dead_code is
// allowed — with the clippy -D warnings gate kept green.
#![allow(dead_code)]

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// ADR-0018 canonical agent name (`flywheel-<slug>-<role><seq>` or a
/// registered alias). Branded so a verdict actor cannot be a free string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AgentName(String);

impl AgentName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Acceptance-criteria shape: is the AC a checkable command/test, or a
/// judgment ("review", "design", "does this feel right")?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcShape {
    Checkable,
    Judgment,
}

/// Blast radius of a bead (ADR-0019 Spec 1). High when the bead overlaps
/// reserved paths or its AC is judgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Blast {
    Normal,
    High,
}

impl Blast {
    /// ADR-0019 derivation rule: High iff overlapping reserved paths OR the AC
    /// is judgment.
    pub fn of(overlap: bool, ac: AcShape) -> Blast {
        if overlap || ac == AcShape::Judgment {
            Blast::High
        } else {
            Blast::Normal
        }
    }
}

/// The unit of work (ADR-0019 Spec 1). Pure data; no IO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadUnit {
    pub id: String,
    pub title: String,
    /// br priority scale, 0-4.
    pub priority: u8,
    /// VERIFY command. Required; missing/empty is refuse-to-dispatch.
    pub verify: String,
    pub blast: Blast,
    pub owner: Option<AgentName>,
    /// SHA of the commit whose message carries the bead id (SHA rule).
    pub head_sha: Option<String>,
}

/// Who produced the proof (ADR-0019 Spec 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VerdictKind {
    /// The LOOP executed VERIFY and wrote the receipt itself.
    CommandVerified,
    /// Independent verifier ran the command/test.
    UnitTestVerified,
    /// Independent verifier exercised the live surface.
    LiveVerified,
    /// Independent verifier signed a judgment-AC bead (no command exists).
    ReviewerSigned,
    /// Worker self-report; legal only where the loop cannot run VERIFY.
    WorkerReceipt,
    /// Loop state, not a pass.
    VerifierBlocked,
    /// Loop state, not a pass.
    VerifierFailed,
}

impl VerdictKind {
    /// Every kind; the exhaustive-match surface for `legal_close`.
    pub const ALL: [VerdictKind; 7] = [
        VerdictKind::CommandVerified,
        VerdictKind::UnitTestVerified,
        VerdictKind::LiveVerified,
        VerdictKind::ReviewerSigned,
        VerdictKind::WorkerReceipt,
        VerdictKind::VerifierBlocked,
        VerdictKind::VerifierFailed,
    ];

    /// br gate name (kebab-cased `VerdictKind`, ADR-0019 Spec 1). Only PASS
    /// verdicts become gate rows; Blocked/Failed are loop state, but the names
    /// exist so the mapping is total.
    pub fn gate_name(self) -> &'static str {
        match self {
            VerdictKind::CommandVerified => "command-verified",
            VerdictKind::UnitTestVerified => "unit-test-verified",
            VerdictKind::LiveVerified => "live-verified",
            VerdictKind::ReviewerSigned => "reviewer-signed",
            VerdictKind::WorkerReceipt => "worker-receipt",
            VerdictKind::VerifierBlocked => "verifier-blocked",
            VerdictKind::VerifierFailed => "verifier-failed",
        }
    }

    /// Inverse of `gate_name` — parse a br gate row's gate name back into a
    /// kind. Unknown names -> `None` (not a verdict row).
    pub fn from_gate_name(name: &str) -> Option<VerdictKind> {
        Some(match name {
            "command-verified" => VerdictKind::CommandVerified,
            "unit-test-verified" => VerdictKind::UnitTestVerified,
            "live-verified" => VerdictKind::LiveVerified,
            "reviewer-signed" => VerdictKind::ReviewerSigned,
            "worker-receipt" => VerdictKind::WorkerReceipt,
            "verifier-blocked" => VerdictKind::VerifierBlocked,
            "verifier-failed" => VerdictKind::VerifierFailed,
            _ => return None,
        })
    }
}

/// A recorded proof (ADR-0019 Spec 1). Non-Option by construction: a close
/// with no verdict has no representation — `{status: closed, verdict: none}`
/// cannot compile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub kind: VerdictKind,
    pub bead_id: String,
    pub sha: String,
    pub at: DateTime<Utc>,
    pub actor: AgentName,
    pub receipt_path: PathBuf,
    pub verify_cmd: String,
}

/// Cheap-band classifier (ADR-0019 Spec 1): VERIFY is loop-runnable when it
/// is a single command line with an allowlisted executable (cargo/test/
/// pytest/python/bounded shell) estimated cheap — the loop can execute it
/// headless under the build slot within the bead's TIMEBOX. Everything else
/// is the non-runnable band (worker receipt + two-tick grace, or independent
/// verification).
pub fn verify_is_loop_runnable(verify: &str) -> bool {
    let Some((exec, args)) = parse_verify_command(verify) else {
        return false;
    };
    allowlisted_exec(&exec) && estimated_cheap(&exec, &args)
}

/// Parse a VERIFY line into `(executable, args)`. `None` when empty,
/// multi-line, or shell composition (`&&`, `||`, `|`, `;`, backticks,
/// `$( )`) — composition is never loop-runnable.
fn parse_verify_command(verify: &str) -> Option<(String, Vec<String>)> {
    let t = verify.trim();
    if t.is_empty()
        || t.contains('\n')
        || t.contains("&&")
        || t.contains("||")
        || t.contains('|')
        || t.contains(';')
        || t.contains('`')
        || t.contains("$(")
    {
        return None;
    }
    let mut parts = t.split_whitespace();
    let exec = parts.next()?.to_string();
    let args = parts.map(String::from).collect();
    Some((exec, args))
}

/// Executables the loop may run headless on the cheap band. A "bounded
/// shell" (sh/bash) is allowed only when the invocation carries an explicit
/// `timeout` bound — an unbounded shell is a time bomb.
fn allowlisted_exec(exec: &str) -> bool {
    matches!(
        exec,
        "cargo" | "test" | "pytest" | "python" | "python3" | "timeout" | "sh" | "bash"
    )
}

/// Known-heavy markers: whole-workspace matrices, long waits, live servers.
fn is_heavy_marker(arg: &str) -> bool {
    arg == "--workspace"
        || arg == "--all-features"
        || arg == "--all-targets"
        || arg == "--ignored"
        || arg.contains("verify-gates")
        || arg.starts_with("sleep")
        || arg.starts_with("serve")
}

/// Estimated-cheap heuristic: a targeted, bounded check. Whole-workspace
/// matrices and unbounded shells are time bombs — non-runnable band.
fn estimated_cheap(exec: &str, args: &[String]) -> bool {
    match exec {
        // cargo: a TARGETED test run (`-p <pkg>` and/or a test filter after
        // `test`); never the whole workspace matrix or a bare `cargo test`.
        "cargo" => {
            let Some(test_pos) = args.iter().position(|a| a == "test") else {
                return false; // cargo without `test` is not a verification
            };
            !args.iter().any(|a| is_heavy_marker(a))
                && (args[test_pos + 1..]
                    .iter()
                    .any(|a| a == "-p" || a.starts_with("-p") || a.starts_with("--package"))
                    || args[test_pos + 1..].iter().any(|a| !a.starts_with('-')))
        }
        // pytest / test runners / python: targeted by default — reject the
        // known-heavy markers.
        "pytest" | "test" | "python" | "python3" => !args.iter().any(|a| is_heavy_marker(a)),
        // Bounded shell: must carry an explicit `timeout` bound (quoted
        // args split on whitespace, so match the token as a substring).
        "sh" | "bash" => args.iter().any(|a| a.contains("timeout")),
        // An explicit `timeout` wrapper is loop-runnable by definition (the
        // bound is the caller's call).
        "timeout" => true,
        _ => false,
    }
}

/// Legal-close table (ADR-0019 Spec 1, amended 2026-08-15):
///
/// | Condition | Legal kinds |
/// |---|---|
/// | priority >= 2, blast Normal, VERIFY loop-runnable | `CommandVerified` (loop-run) ONLY |
/// | priority >= 2, blast Normal, VERIFY not loop-runnable | `WorkerReceipt` (+ two-tick grace), or `UnitTestVerified` / `LiveVerified` |
/// | priority <= 1 or blast High | `UnitTestVerified`, `LiveVerified` only |
/// | AC is judgment | `ReviewerSigned` only (recording `unit-test-verified` for a judgment bead is a FALSE verdict) |
/// | any | never `VerifierFailed`, never `VerifierBlocked`, never missing |
///
/// The match below is exhaustive over `VerdictKind` with no wildcard: a new
/// kind without a `legal_close` case does not compile.
pub fn legal_close(verdict: &Verdict, bead: &BeadUnit, ac: AcShape) -> bool {
    let runnable = verify_is_loop_runnable(&bead.verify);
    // Exhaustive over VerdictKind, no wildcard: a new kind without a
    // legal_close case does not compile (acceptance gate, bd-w1-types-vbez).
    match verdict.kind {
        // Row 1 — cheap band: loop-run CommandVerified ONLY.
        VerdictKind::CommandVerified => {
            ac == AcShape::Checkable
                && bead.priority >= 2
                && bead.blast == Blast::Normal
                && runnable
        }
        // Row 2 — non-runnable band: WorkerReceipt (+ two-tick grace) or an
        // independent unit/live verification.
        VerdictKind::WorkerReceipt => {
            ac == AcShape::Checkable
                && bead.priority >= 2
                && bead.blast == Blast::Normal
                && !runnable
        }
        // Row 3 — P0/P1 or High blast: independent verification only.
        VerdictKind::UnitTestVerified | VerdictKind::LiveVerified => {
            ac == AcShape::Checkable && (bead.priority <= 1 || bead.blast == Blast::High)
        }
        // Row 4 — judgment AC: ReviewerSigned only. Recording any other kind
        // for a judgment bead is a false verdict.
        VerdictKind::ReviewerSigned => ac == AcShape::Judgment,
        // Row 5 — never a pass.
        VerdictKind::VerifierBlocked | VerdictKind::VerifierFailed => false,
    }
}

// ============================================================================
// br-gate verdict store + receipts (ADR-0019 Spec 1; bd-w1-gate-store-h2ur)
// The types and the matrix above stay pure; IO lives in this section.
// ============================================================================

use std::path::Path;
use std::process::Command as StdCommand;

/// Loop-reporter arm flag. Only the durable loop arms it (durable.rs). A
/// worker-invoked `br gate report` for a loop-owned bead is rejected while
/// unarmed — workers never write verdicts (ADR-0019 Spec 1).
static LOOP_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Arm the loop reporter. Called once by the durable loop at startup
/// (durable.rs, bd-w1-reopen-f6tw wiring). Idempotent.
pub fn arm_loop_reporter() {
    LOOP_ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn loop_armed() -> bool {
    LOOP_ARMED.load(std::sync::atomic::Ordering::SeqCst)
}

/// One gate row from `br gate list <id> --json` history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateRow {
    pub id: i64,
    pub issue_id: String,
    pub from_status: String,
    pub to_status: String,
    pub status_revision: i64,
    pub gate: String,
    pub provider: String,
    pub passed: bool,
    pub note: String,
    pub recorded_by: String,
    pub recorded_at: String,
}

/// `br gate list <id> --json` (unknown fields — legacy_results,
/// gated_transitions, … — are ignored by serde).
#[derive(Clone, Debug, Deserialize)]
pub struct GateList {
    pub issue_id: String,
    pub current_status: String,
    pub history: Vec<GateRow>,
}

/// Run `br` with `args` in `cwd`; stdout on success, stderr in the error.
/// Same subprocess style as the existing br calls in the loop (no nested
/// block_on).
fn run_br(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = StdCommand::new("br")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| anyhow::anyhow!("br not found: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "br {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Record a PASS verdict (ADR-0019 Spec 1): `br gate report <id> --gate
/// <kind> --provider <actor> --status pass --note "sha=<sha> receipt=<path>"`.
///
/// SPIKE VERDICT (bd-w1-policy-spike-nq1p, br 0.2.22, 2026-08-16): `br gate
/// report --to closed` does NOT transition the issue — verdicts are append-only
/// `gate_result_history` rows, a later FAIL cannot revert a sticky PASS, and
/// `br close` ignores gate satisfaction (policy.yaml `require_all` is advisory).
/// Gate policy is DETECTION, not PREVENTION: the loop-side reopen (w1-reopen)
/// is the enforcement floor. Revisit only if br grows native close gating.
///
/// Loop-only: refuses while the loop reporter is not armed (a worker-invoked
/// report on a loop-owned bead is rejected), and refuses Blocked/Failed —
/// those are loop state, never gate rows.
pub fn report_pass(cwd: &Path, verdict: &Verdict) -> anyhow::Result<()> {
    if !loop_armed() {
        anyhow::bail!(
            "verdict reporting is loop-only: a worker-invoked br gate report for a loop-owned bead is rejected"
        );
    }
    if matches!(
        verdict.kind,
        VerdictKind::VerifierBlocked | VerdictKind::VerifierFailed
    ) {
        anyhow::bail!(
            "only PASS verdicts are recorded; {:?} is loop state, never a gate row",
            verdict.kind
        );
    }
    let gate = verdict.kind.gate_name();
    let note = format!(
        "sha={} receipt={}",
        verdict.sha,
        verdict.receipt_path.display()
    );
    let stdout = run_br(
        cwd,
        &[
            "gate",
            "report",
            &verdict.bead_id,
            "--gate",
            gate,
            "--provider",
            verdict.actor.as_str(),
            "--status",
            "pass",
            "--note",
            &note,
        ],
    )?;
    let _ = stdout;
    Ok(())
}

/// Wrap `br gate list <id> --json`.
pub fn list_gates(cwd: &Path, bead_id: &str) -> anyhow::Result<GateList> {
    let stdout = run_br(cwd, &["gate", "list", bead_id, "--json"])?;
    Ok(serde_json::from_str(stdout.trim())?)
}

/// SHA rule (ADR-0019 Spec 1): the verdict must bind to the commit whose
/// message carries the bead id. `git log --all -1 --grep=<id> --format=%H`,
/// no bare-HEAD fallback: no commit carries the id -> the close is illegal
/// regardless of verdicts.
pub fn derive_head_sha(cwd: &Path, bead_id: &str) -> anyhow::Result<String> {
    let out = StdCommand::new("git")
        .args([
            "log",
            "--all",
            "-1",
            &format!("--grep={bead_id}"),
            "--format=%H",
        ])
        .current_dir(cwd)
        .output()
        .map_err(|e| anyhow::anyhow!("git not found: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git log --grep failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        anyhow::bail!(
            "SHA rule: no commit carries the bead id {bead_id} (git log --all --grep) — \
             close is illegal regardless of verdicts"
        );
    }
    Ok(sha)
}

/// Rebuild a `BeadUnit` from `br show <id> --json`: priority, title, and the
/// `## VERIFY` fence parsed from the description (shared parser in durable.rs,
/// bd-w1-verify-fence). Blast/AC-shape are not in br; the caller supplies them
/// (the loop derives blast from reservation overlap + AC shape at ready-parse).
pub fn bead_from_br_show(cwd: &Path, bead_id: &str) -> anyhow::Result<BeadUnit> {
    #[derive(Deserialize)]
    struct BrShow {
        id: String,
        title: String,
        priority: u8,
        description: String,
    }
    let stdout = run_br(cwd, &["show", bead_id, "--json"])?;
    let v: serde_json::Value = serde_json::from_str(stdout.trim())?;
    let row: BrShow = serde_json::from_value(v[0].clone())?;
    let verify = crate::durable::parse_verify_fence(&row.description).ok_or_else(|| {
        anyhow::anyhow!("bead {bead_id}: missing/malformed ## VERIFY fence — refuse to dispatch")
    })?;
    Ok(BeadUnit {
        id: row.id,
        title: row.title,
        priority: row.priority,
        verify,
        blast: Blast::Normal,
        owner: None,
        head_sha: None,
    })
}

/// `note` key=value token helpers (ADR-0019 Spec 1: sha + receipt ride in the
/// report `--note`).
fn note_token<'a>(note: &'a str, key: &str) -> Option<&'a str> {
    note.split_whitespace()
        .find_map(|tok| tok.strip_prefix(key)?.strip_prefix('='))
}

fn note_has_sha(note: &str, sha: &str) -> bool {
    note_token(note, "sha").is_some_and(|v| v == sha)
}

fn note_receipt_path(note: &str) -> Option<PathBuf> {
    note_token(note, "receipt").map(PathBuf::from)
}

/// Parse a gate row's `recorded_at` (RFC3339 or the br human format
/// `%Y-%m-%d %H:%M:%S`).
fn parse_recorded_at(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|n| n.and_utc())
        })
}

/// `flywheel ledger check` core: wrap `br gate list <id> --json`, apply
/// `legal_close`, and report whether any history row is a legal verdict bound
/// to `sha`. Exits 0 only on a legal verdict for the current SHA (the CLI
/// layer maps `Ok(false)` to a non-zero exit).
pub fn ledger_check(cwd: &Path, bead: &BeadUnit, ac: AcShape, sha: &str) -> anyhow::Result<bool> {
    let list = list_gates(cwd, &bead.id)?;
    for row in &list.history {
        if !row.passed {
            continue;
        }
        let Some(kind) = VerdictKind::from_gate_name(&row.gate) else {
            continue;
        };
        // Blocked/Failed are loop state, never gate rows; if a row somehow
        // carries them it is not a pass verdict.
        if matches!(
            kind,
            VerdictKind::VerifierBlocked | VerdictKind::VerifierFailed
        ) {
            continue;
        }
        // The verdict must bind to this SHA (SHA rule): the note carries
        // `sha=<sha>`.
        if !note_has_sha(&row.note, sha) {
            continue;
        }
        let Some(at) = parse_recorded_at(&row.recorded_at) else {
            continue;
        };
        let verdict = Verdict {
            kind,
            bead_id: bead.id.clone(),
            sha: sha.to_string(),
            at,
            actor: AgentName::new(row.provider.clone()),
            receipt_path: note_receipt_path(&row.note).unwrap_or_default(),
            verify_cmd: bead.verify.clone(),
        };
        if legal_close(&verdict, bead, ac) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Worker helper: write ONLY the receipt file
/// `.flywheel/receipts/<bead-id>-<sha>.<kind>.txt`. Refuses empty content.
/// Workers never write verdicts — this helper writes evidence, not proof.
pub fn write_receipt(
    cwd: &Path,
    bead_id: &str,
    sha: &str,
    kind: VerdictKind,
    content: &str,
) -> anyhow::Result<PathBuf> {
    if content.trim().is_empty() {
        anyhow::bail!("receipt refuses empty content");
    }
    // Anti-forgery (ADR-0019 Spec 1): command-verified receipts are written
    // by the LOOP only — a lying worker cannot forge what it does not write.
    // The worker helper refuses this kind outright.
    if matches!(kind, VerdictKind::CommandVerified) {
        anyhow::bail!(
            "command-verified receipts are written by the LOOP only (anti-forgery, ADR-0019 Spec 1): workers never write this kind"
        );
    }
    let dir = cwd.join(".flywheel").join("receipts");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{bead_id}-{sha}.{}.txt", kind.gate_name()));
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Machine receipt writer — LOOP ONLY (armed-gated like `report_pass`):
/// `.flywheel/receipts/<bead-id>-<sha>.command-verified.txt`. The loop
/// executes VERIFY itself and writes this file; workers never write this
/// kind (anti-forgery by construction, ADR-0019 Spec 1).
pub fn write_command_verified_receipt(
    cwd: &Path,
    bead_id: &str,
    sha: &str,
    content: &str,
) -> anyhow::Result<PathBuf> {
    if !loop_armed() {
        anyhow::bail!(
            "command-verified receipts are written by the LOOP only (anti-forgery, ADR-0019 Spec 1)"
        );
    }
    if content.trim().is_empty() {
        anyhow::bail!("receipt refuses empty content");
    }
    let dir = cwd.join(".flywheel").join("receipts");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{bead_id}-{sha}.command-verified.txt"));
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Close-detect decision (ADR-0019, bd-w1-reopen-f6tw): what the loop must
/// do when a dispatched bead left the tracker as closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseCheck {
    /// A legal verdict row binds the derived sha — leave the bead closed.
    Legal { sha: String },
    /// No legal verdict row for the derived sha. `receipt_eligible` = the
    /// bead MAY close via WorkerReceipt (the receipt races the close write)
    /// — the loop grants TWO consecutive missing ticks before reopening.
    /// Non-eligible -> reopen now.
    Missing { sha: String, receipt_eligible: bool },
    /// No commit carries the bead id — the close is illegal regardless of
    /// verdicts; reopen now.
    NoCommit,
}

/// The close-detect check: SHA rule first (`derive_head_sha`), then the
/// ledger (`ledger_check`). Pure decision over br/git — the loop's reopen
/// leaf turns a non-`Legal` verdict into `br reopen` + one mail per
/// (bead, sha).
pub fn close_check(cwd: &Path, bead_id: &str) -> anyhow::Result<CloseCheck> {
    let sha = match derive_head_sha(cwd, bead_id) {
        Ok(s) => s,
        // "SHA rule: no commit carries the bead id" — the close is illegal.
        // Anything else (git missing, git log failure) is an environment
        // error and must propagate, never masquerade as a verdict.
        Err(e) => {
            if format!("{e:#}").contains("SHA rule") {
                return Ok(CloseCheck::NoCommit);
            }
            return Err(e);
        }
    };
    // A closed bead without a legal VERIFY fence has no receipt path either —
    // fail closed: reopen it (the fence is required before dispatch).
    let bead = match bead_from_br_show(cwd, bead_id) {
        Ok(b) => b,
        Err(_) => {
            return Ok(CloseCheck::Missing {
                sha,
                receipt_eligible: false,
            });
        }
    };
    if ledger_check(cwd, &bead, AcShape::Checkable, &sha)? {
        return Ok(CloseCheck::Legal { sha });
    }
    // Is WorkerReceipt a legal close for this bead? (priority >= 2, Normal
    // blast, VERIFY not loop-runnable — row 2 of the legal-close table).
    let receipt = Verdict {
        kind: VerdictKind::WorkerReceipt,
        bead_id: bead_id.to_string(),
        sha: sha.clone(),
        at: Utc::now(),
        actor: AgentName::new("flywheel-toron-loop1"),
        receipt_path: PathBuf::new(),
        verify_cmd: bead.verify.clone(),
    };
    let receipt_eligible = legal_close(&receipt, &bead, AcShape::Checkable);
    Ok(CloseCheck::Missing {
        sha,
        receipt_eligible,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;

    use super::*;

    fn bead(priority: u8, blast: Blast, runnable: bool) -> BeadUnit {
        BeadUnit {
            id: "bd-test-1".into(),
            title: "test bead".into(),
            priority,
            verify: if runnable {
                "cargo test -p flywheel verdict::".into()
            } else {
                "cargo test -p flywheel verdict:: && sleep 3600".into()
            },
            blast,
            owner: None,
            head_sha: None,
        }
    }

    fn verdict(kind: VerdictKind) -> Verdict {
        Verdict {
            kind,
            bead_id: "bd-test-1".into(),
            sha: "deadbeef".into(),
            at: Utc::now(),
            actor: AgentName::new("flywheel-toron-coder1"),
            receipt_path: PathBuf::from("/tmp/receipt.txt"),
            verify_cmd: "cargo test -p flywheel verdict::".into(),
        }
    }

    /// The ADR-0019 Spec 1 table, transcribed as a predicate. This is the
    /// oracle the implementation is checked against on every cell.
    fn expected(
        kind: VerdictKind,
        ac: AcShape,
        priority: u8,
        blast: Blast,
        runnable: bool,
    ) -> bool {
        use AcShape::{Checkable, Judgment};
        use Blast::{High, Normal};
        use VerdictKind::*;
        match kind {
            // Row 4 — judgment AC: ReviewerSigned only, regardless of
            // priority / blast / runnability.
            ReviewerSigned => ac == Judgment,
            // Row 5 — never a pass.
            VerifierBlocked | VerifierFailed => false,
            // Row 1 — cheap band: CommandVerified (loop-run) ONLY.
            CommandVerified => ac == Checkable && priority >= 2 && blast == Normal && runnable,
            // Row 2 — non-runnable band: WorkerReceipt or unit/live verified.
            WorkerReceipt => ac == Checkable && priority >= 2 && blast == Normal && !runnable,
            // Row 3 — P0/P1 or High blast: independent verification only.
            UnitTestVerified | LiveVerified => ac == Checkable && (priority <= 1 || blast == High),
        }
    }

    /// Acceptance: table test covers EVERY kind x priority(0..4) x blast x
    /// AC-shape x loop-runnability cell (7 * 5 * 2 * 2 * 2 = 280 cells).
    #[test]
    fn legal_close_matches_table_over_every_cell() {
        for kind in VerdictKind::ALL {
            for priority in 0..=4u8 {
                for blast in [Blast::Normal, Blast::High] {
                    for ac in [AcShape::Checkable, AcShape::Judgment] {
                        for runnable in [true, false] {
                            let bead = bead(priority, blast, runnable);
                            let v = verdict(kind);
                            let want = expected(kind, ac, priority, blast, runnable);
                            let got = legal_close(&v, &bead, ac);
                            assert_eq!(
                                got, want,
                                "kind={kind:?} priority={priority} blast={blast:?} ac={ac:?} runnable={runnable}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn worker_receipt_illegal_on_loop_runnable_verify() {
        // Row 2 vs Row 1: WorkerReceipt on a loop-runnable VERIFY is a
        // forged-proof path — must be illegal.
        let bead = bead(3, Blast::Normal, /* runnable */ true);
        let v = verdict(VerdictKind::WorkerReceipt);
        assert!(!legal_close(&v, &bead, AcShape::Checkable));
    }

    #[test]
    fn command_verified_illegal_outside_cheap_band() {
        // P0 bead (needs independent verification) with a loop-runnable VERIFY.
        let p0_bead = bead(0, Blast::Normal, true);
        let v = verdict(VerdictKind::CommandVerified);
        assert!(!legal_close(&v, &p0_bead, AcShape::Checkable));
        // High blast (overlap, checkable AC) also excludes the loop's own run.
        let high_blast_bead = bead(3, Blast::High, true);
        let v = verdict(VerdictKind::CommandVerified);
        assert!(!legal_close(&v, &high_blast_bead, AcShape::Checkable));
    }

    #[test]
    fn judgment_ac_closes_only_with_reviewer_signed() {
        // Recording unit-test-verified for a judgment bead is a FALSE verdict.
        for kind in VerdictKind::ALL {
            let bead = bead(1, Blast::of(false, AcShape::Judgment), true);
            let v = verdict(kind);
            let want = kind == VerdictKind::ReviewerSigned;
            assert_eq!(
                legal_close(&v, &bead, AcShape::Judgment),
                want,
                "kind={kind:?}"
            );
        }
    }

    #[test]
    fn blocked_and_failed_are_never_legal() {
        for priority in 0..=4u8 {
            for blast in [Blast::Normal, Blast::High] {
                for ac in [AcShape::Checkable, AcShape::Judgment] {
                    for runnable in [true, false] {
                        let bead = bead(priority, blast, runnable);
                        for kind in [VerdictKind::VerifierBlocked, VerdictKind::VerifierFailed] {
                            let v = verdict(kind);
                            assert!(!legal_close(&v, &bead, ac));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cheap_band_is_loop_run_only() {
        // p>=2 + Normal + runnable: only CommandVerified closes it. An
        // independent verifier rerunning one cheap command is ceremony
        // (ADR-0019 Spec 4).
        let bead = bead(3, Blast::Normal, true);
        for kind in VerdictKind::ALL {
            let v = verdict(kind);
            let want = kind == VerdictKind::CommandVerified;
            assert_eq!(legal_close(&v, &bead, AcShape::Checkable), want);
        }
    }

    #[test]
    fn verify_is_loop_runnable_predicate() {
        assert!(verify_is_loop_runnable("cargo test -p flywheel verdict::"));
        assert!(!verify_is_loop_runnable(""));
        assert!(!verify_is_loop_runnable("   "));
        assert!(!verify_is_loop_runnable("cargo test && sleep 3600"));
        assert!(!verify_is_loop_runnable("cargo test || cargo build"));
        assert!(!verify_is_loop_runnable("cargo test | tee out.txt"));
        assert!(!verify_is_loop_runnable("cargo test; echo done"));
        assert!(!verify_is_loop_runnable("cargo test\ncargo build"));
        assert!(!verify_is_loop_runnable("cargo test `echo x`"));
        assert!(!verify_is_loop_runnable("cargo test $(echo x)"));
    }

    // ---- bd-w1-loop-run-verify-mbbe: loop-runnable classifier ----

    /// Targeted, allowlisted checks are loop-runnable.
    #[test]
    fn loop_run_verify_classifier_accepts_targeted_checks() {
        assert!(verify_is_loop_runnable("cargo test -p flywheel verdict::"));
        assert!(verify_is_loop_runnable("cargo test -p flywheel loop_run_verify"));
        assert!(verify_is_loop_runnable("cargo test --package flywheel verifier"));
        assert!(verify_is_loop_runnable("pytest tests/test_gate.py"));
        assert!(verify_is_loop_runnable("python -m pytest tests/test_x.py"));
        assert!(verify_is_loop_runnable("timeout 300 cargo test -p flywheel verifier"));
        assert!(verify_is_loop_runnable("sh -c \"timeout 60 cargo test -p flywheel verifier\""));
        assert!(verify_is_loop_runnable("test -f Cargo.toml"));
    }

    /// Whole-workspace matrices, unbounded shells, and non-verification
    /// commands are NOT loop-runnable.
    #[test]
    fn loop_run_verify_classifier_rejects_heavy_and_unbounded() {
        assert!(!verify_is_loop_runnable("cargo test --workspace"));
        assert!(!verify_is_loop_runnable("cargo test --all-features"));
        assert!(!verify_is_loop_runnable("cargo test --all-targets"));
        assert!(!verify_is_loop_runnable("cargo test"));
        assert!(!verify_is_loop_runnable("cargo test --lib"));
        assert!(!verify_is_loop_runnable("cargo build"));
        assert!(!verify_is_loop_runnable("bash scripts/verify-gates.sh"));
        assert!(!verify_is_loop_runnable("sh -c 'cargo test --workspace'"));
        assert!(!verify_is_loop_runnable("sleep 3600"));
        assert!(!verify_is_loop_runnable("git push"));
        assert!(!verify_is_loop_runnable("npm run e2e"));
    }

    /// Shell composition and empty VERIFY are never loop-runnable.
    #[test]
    fn loop_run_verify_classifier_rejects_composition() {
        assert!(!verify_is_loop_runnable(""));
        assert!(!verify_is_loop_runnable("   "));
        assert!(!verify_is_loop_runnable("cargo test -p flywheel verifier && echo done"));
        assert!(!verify_is_loop_runnable("cargo test | tee out.txt"));
        assert!(!verify_is_loop_runnable("cargo test; echo done"));
        assert!(!verify_is_loop_runnable("cargo test\ncargo build"));
        assert!(!verify_is_loop_runnable("cargo test `echo x`"));
        assert!(!verify_is_loop_runnable("cargo test $(echo x)"));
    }

    /// Anti-forgery: the WORKER receipt helper refuses CommandVerified
    /// outright (no arm dependency — always refused).
    #[test]
    fn worker_ledger_receipt_refuses_command_verified_kind() {
        let dir = std::env::temp_dir().join(format!("lr-receipt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = write_receipt(&dir, "bd-x", "abc", VerdictKind::CommandVerified, "out")
            .unwrap_err();
        assert!(
            err.to_string().contains("LOOP only"),
            "worker must not write command-verified: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blast_derivation_high_iff_overlap_or_judgment() {
        assert_eq!(Blast::of(false, AcShape::Checkable), Blast::Normal);
        assert_eq!(Blast::of(true, AcShape::Checkable), Blast::High);
        assert_eq!(Blast::of(false, AcShape::Judgment), Blast::High);
        assert_eq!(Blast::of(true, AcShape::Judgment), Blast::High);
    }

    #[test]
    fn every_kind_has_a_distinct_gate_name() {
        let mut names: Vec<&str> = VerdictKind::ALL.iter().map(|k| k.gate_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), VerdictKind::ALL.len());
        for k in VerdictKind::ALL {
            assert!(!k.gate_name().is_empty());
            assert_eq!(k.gate_name(), k.gate_name().to_ascii_lowercase());
        }
    }

    #[test]
    fn agent_name_brands_non_empty_strings() {
        let n = AgentName::new("flywheel-toron-coder1");
        assert_eq!(n.as_str(), "flywheel-toron-coder1");
        assert_eq!(n.to_string(), "flywheel-toron-coder1");
        assert_ne!(n, AgentName::new("flywheel-toron-coder2"));
    }

    // ---- bd-w1-gate-store-h2ur: br gate store + receipts + loop guard ----

    /// Scratch workspace: git repo + br tracker + gate policy (spike shape,
    /// GreenStork 2026-08-16) + one P3 issue with a `## VERIFY` fence + a
    /// commit whose message carries the bead id (SHA rule). Returns
    /// `(dir, bead_id)`.
    fn scratch_workspace(tag: &str) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let init = StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init");
        let bi = StdCommand::new("br")
            .arg("init")
            .current_dir(&dir)
            .output()
            .expect("br init");
        assert!(
            bi.status.success(),
            "br init: {}",
            String::from_utf8_lossy(&bi.stderr)
        );
        std::fs::write(
            dir.join(".beads/policy.yaml"),
            "workflow:\n  transitions:\n    open:\n      - closed\n  gates:\n    open -> closed:\n      require_all:\n        - command-verified\n",
        )
        .unwrap();
        let created = StdCommand::new("br")
            .args([
                "create",
                "--title",
                "gate roundtrip",
                "--slug",
                "gate-rt-1",
                "-p",
                "3",
                "-d",
                "work\n\n## VERIFY\n```\ncargo test -p flywheel verdict::\n```",
            ])
            .current_dir(&dir)
            .output()
            .expect("br create");
        assert!(
            created.status.success(),
            "br create: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let list = StdCommand::new("br")
            .args(["list", "--json"])
            .current_dir(&dir)
            .output()
            .expect("br list");
        let v: serde_json::Value = serde_json::from_slice(&list.stdout).expect("br list json");
        let bead_id = v["issues"][0]["id"].as_str().expect("bead id").to_string();
        std::fs::write(dir.join("work.txt"), "work\n").unwrap();
        let add = StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .output()
            .expect("git add");
        assert!(add.status.success());
        let cm = StdCommand::new("git")
            .args([
                "-c",
                "user.email=test@test",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                &format!("feat: gate roundtrip ({bead_id})"),
            ])
            .current_dir(&dir)
            .output()
            .expect("git commit");
        assert!(
            cm.status.success(),
            "git commit: {}",
            String::from_utf8_lossy(&cm.stderr)
        );
        (dir, bead_id)
    }

    /// Acceptance round-trip: `br gate report` then ledger check legal on the
    /// same bead — plus the loop-only reporter guard on both paths
    /// (worker-invoked report blocked, loop path accepted).
    #[test]
    fn loop_only_reporter_guard_blocks_workers_then_loop_roundtrips() {
        let (dir, bead_id) = scratch_workspace("gate-roundtrip");
        let sha = derive_head_sha(&dir, &bead_id).unwrap();
        let bead = bead_from_br_show(&dir, &bead_id).unwrap();
        assert_eq!(bead.priority, 3);
        assert_eq!(bead.verify, "cargo test -p flywheel verdict::");
        let v = Verdict {
            kind: VerdictKind::CommandVerified,
            bead_id: bead_id.clone(),
            sha: sha.clone(),
            at: Utc::now(),
            actor: AgentName::new("flywheel-toron-loop1"),
            receipt_path: dir.join(".flywheel/receipts/x.txt"),
            verify_cmd: bead.verify.clone(),
        };
        // worker path: the loop reporter is unarmed -> rejected.
        let err = report_pass(&dir, &v).unwrap_err();
        assert!(err.to_string().contains("loop-only"), "{err}");
        // Anti-forgery (bd-w1-loop-run-verify-mbbe): the machine receipt for
        // the loop-run band is written by the LOOP only — refused while
        // unarmed (a worker cannot forge what it does not write).
        let err = write_command_verified_receipt(&dir, &bead_id, &sha, "ok\n").unwrap_err();
        assert!(
            err.to_string().contains("LOOP only"),
            "unarmed loop writer must refuse: {err}"
        );
        // loop path: armed -> recorded as a PASS gate row.
        arm_loop_reporter();
        report_pass(&dir, &v).unwrap();
        let list = list_gates(&dir, &bead_id).unwrap();
        assert_eq!(list.history.len(), 1);
        assert!(list.history[0].passed);
        assert_eq!(list.history[0].gate, "command-verified");
        assert!(note_has_sha(&list.history[0].note, &sha));
        // ledger check: legal for the current SHA, not for a wrong SHA.
        assert!(ledger_check(&dir, &bead, AcShape::Checkable, &sha).unwrap());
        assert!(
            !ledger_check(
                &dir,
                &bead,
                AcShape::Checkable,
                "0000000000000000000000000000000000000000"
            )
            .unwrap()
        );
        // Anti-forgery (armed path): the loop's machine receipt writes the
        // exact filename once armed.
        let path =
            write_command_verified_receipt(&dir, &bead_id, &sha, "cargo test ... ok\n").unwrap();
        assert_eq!(
            path,
            dir.join(format!(".flywheel/receipts/{bead_id}-{sha}.command-verified.txt"))
        );
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_head_sha_refuses_when_no_commit_carries_the_id() {
        let (dir, _id) = scratch_workspace("sha-rule");
        let err = derive_head_sha(&dir, "bd-never-committed").unwrap_err();
        assert!(err.to_string().contains("SHA rule"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn receipt_helper_writes_exact_filename_and_refuses_empty() {
        let dir = std::env::temp_dir().join(format!("gate-receipt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_receipt(
            &dir,
            "bd-x-1",
            "abc123",
            VerdictKind::WorkerReceipt,
            "all tests passed\n",
        )
        .unwrap();
        assert_eq!(
            path,
            dir.join(".flywheel/receipts/bd-x-1-abc123.worker-receipt.txt")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "all tests passed\n"
        );
        let err = write_receipt(
            &dir,
            "bd-x-1",
            "abc123",
            VerdictKind::WorkerReceipt,
            "   \n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_names_roundtrip_through_from_gate_name() {
        for kind in VerdictKind::ALL {
            assert_eq!(VerdictKind::from_gate_name(kind.gate_name()), Some(kind));
        }
        assert_eq!(VerdictKind::from_gate_name("not-a-gate"), None);
        assert_eq!(VerdictKind::from_gate_name(""), None);
    }

    #[test]
    fn note_key_value_tokens_parse_sha_and_receipt() {
        let note = "sha=deadbeef receipt=/tmp/r.txt";
        assert!(note_has_sha(note, "deadbeef"));
        assert!(!note_has_sha(note, "beef"));
        assert_eq!(
            note_receipt_path(note).unwrap(),
            PathBuf::from("/tmp/r.txt")
        );
        assert_eq!(note_receipt_path("no tokens"), None);
    }

    // ---- bd-w1-sha-xyhd: SHA invariant at close-detect ----

    /// ADR-0019 SHA-rule fixture (bd-w1-sha-xyhd): a sibling commit landing
    /// AFTER the bead's commit must not hijack the derived sha — the close
    /// sha resolves to the bead's OWN commit, never bare HEAD.
    #[test]
    fn derive_head_sha_resolves_own_commit_when_head_moved() {
        let (dir, bead_id) = scratch_workspace("sha-head-moved");
        let own = derive_head_sha(&dir, &bead_id).unwrap();
        // a sibling lands on top with NO bead id in its message
        std::fs::write(dir.join("sibling.txt"), "sibling\n").unwrap();
        let add = StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .output()
            .expect("git add");
        assert!(add.status.success());
        let cm = StdCommand::new("git")
            .args([
                "-c",
                "user.email=test@test",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                "feat: sibling committed after the bead",
            ])
            .current_dir(&dir)
            .output()
            .expect("git commit");
        assert!(cm.status.success(), "sibling commit");
        // HEAD moved past the bead's commit
        let rp = format!("rev-{}", "parse");
        let head = StdCommand::new("git")
            .args([rp.as_str(), "HEAD"])
            .current_dir(&dir)
            .output()
            .expect("resolve current HEAD sha");
        let head_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert_ne!(own, head_sha, "HEAD moved past the bead commit");
        // the derived sha still resolves to the bead's OWN commit
        assert_eq!(
            derive_head_sha(&dir, &bead_id).unwrap(),
            own,
            "git log --grep resolves the bead's own commit, not bare HEAD"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-0019 (bd-w1-sha-xyhd): NO bare-HEAD fallback anywhere in the
    /// verdict path — in a shared tree bare HEAD is the same sha for
    /// every bead closed between commits and may not contain the closed
    /// bead's work at all. Source-level pin (needle assembled so this test
    /// never trips itself).
    #[test]
    fn sha_rule_never_falls_back_to_bare_head() {
        let needle = format!("rev-{}", "parse");
        for f in ["src/verdict.rs", "src/durable.rs"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert!(
                !src.contains(&needle),
                "{f} must not use the bare-HEAD fallback (ADR-0019)"
            );
        }
    }

    // ---- bd-w1-reopen-f6tw: close-detect reopen decision ----

    /// THE RED-THEN-GREEN FIXTURE: a worker closes with no gate row -> the
    /// close-detect check says MISSING -> the loop reopens -> the bead is
    /// back in `br ready`. (The reopen + mail side lives in the loop's
    /// reopen leaf; the DECISION is this function.)
    #[test]
    fn close_check_reopens_when_worker_closes_without_gate_row() {
        let (dir, bead_id) = scratch_workspace("reopen-red-green");
        let cm = StdCommand::new("br")
            .args(["close", &bead_id, "--reason", "worker says done"])
            .current_dir(&dir)
            .output()
            .expect("br close");
        assert!(cm.status.success(), "br close: {cm:?}");
        match close_check(&dir, &bead_id).unwrap() {
            CloseCheck::Missing {
                sha,
                receipt_eligible,
            } => {
                assert!(
                    !receipt_eligible,
                    "loop-runnable VERIFY is NOT WorkerReceipt-eligible"
                );
                assert!(!sha.is_empty(), "sha derived from the bead's commit");
            }
            other => panic!("expected Missing, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legal verdict row bound to the derived sha -> leave closed.
    /// NOTE: the gate row is recorded via direct `br gate report` (not
    /// `report_pass`) — the loop-only guard is a process-global flag and
    /// arming it here would race lane A's roundtrip test in parallel runs.
    #[test]
    fn close_check_legal_when_gate_row_binds_sha() {
        let (dir, bead_id) = scratch_workspace("reopen-legal");
        let sha = derive_head_sha(&dir, &bead_id).unwrap();
        let gate = StdCommand::new("br")
            .args([
                "gate",
                "report",
                &bead_id,
                "--gate",
                "command-verified",
                "--provider",
                "flywheel-toron-loop1",
                "--status",
                "pass",
                "--note",
                &format!("sha={sha} receipt={}/x.txt", dir.display()),
            ])
            .current_dir(&dir)
            .output()
            .expect("br gate report");
        assert!(gate.status.success(), "br gate report: {gate:?}");
        let cm = StdCommand::new("br")
            .args(["close", &bead_id, "--reason", "verified"])
            .current_dir(&dir)
            .output()
            .expect("br close");
        assert!(cm.status.success(), "br close: {cm:?}");
        assert_eq!(
            close_check(&dir, &bead_id).unwrap(),
            CloseCheck::Legal { sha },
            "gate row binds the sha -> legal"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No commit carries the bead id -> the close is illegal regardless of
    /// verdicts.
    #[test]
    fn close_check_no_commit_is_no_commit() {
        let (dir, bead_id) = scratch_workspace("reopen-nocommit");
        let cm = StdCommand::new("br")
            .args(["close", &bead_id, "--reason", "no commit behind this"])
            .current_dir(&dir)
            .output()
            .expect("br close");
        assert!(cm.status.success(), "br close: {cm:?}");
        // strip the only ref: git log --all now finds no commit
        let strip = StdCommand::new("git")
            .args(["update-ref", "-d", "HEAD"])
            .current_dir(&dir)
            .output()
            .expect("git update-ref");
        assert!(strip.status.success(), "git update-ref: {strip:?}");
        assert_eq!(
            close_check(&dir, &bead_id).unwrap(),
            CloseCheck::NoCommit,
            "no commit -> NoCommit (illegal close)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WorkerReceipt-eligible bead (VERIFY not loop-runnable): the check
    /// grants the two-tick receipt grace instead of an instant reopen.
    #[test]
    fn close_check_grants_receipt_grace_when_worker_receipt_is_legal() {
        let (dir, _bead1) = scratch_workspace("reopen-grace");
        // a second bead whose VERIFY is NOT loop-runnable (shell composition)
        let created = StdCommand::new("br")
            .args([
                "create",
                "--title",
                "receipt bead",
                "--slug",
                "receipt-b-1",
                "-p",
                "3",
                "-d",
                "work\n\n## VERIFY\n```\ncd x && cargo test -p y z\n```",
            ])
            .current_dir(&dir)
            .output()
            .expect("br create");
        assert!(created.status.success(), "br create: {created:?}");
        let list = StdCommand::new("br")
            .args(["list", "--json"])
            .current_dir(&dir)
            .output()
            .expect("br list");
        let v: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list json");
        let bead_id = v["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == "receipt bead")
            .expect("receipt bead")
            .get("id")
            .and_then(|s| s.as_str())
            .expect("id")
            .to_string();
        std::fs::write(dir.join("receipt-work.txt"), "work\n").unwrap();
        let add = StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .output()
            .expect("git add");
        assert!(add.status.success());
        let cm = StdCommand::new("git")
            .args([
                "-c",
                "user.email=test@test",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                &format!("feat: receipt bead ({bead_id})"),
            ])
            .current_dir(&dir)
            .output()
            .expect("git commit");
        assert!(cm.status.success(), "git commit: {cm:?}");
        let close = StdCommand::new("br")
            .args(["close", &bead_id, "--reason", "receipt on the way"])
            .current_dir(&dir)
            .output()
            .expect("br close");
        assert!(close.status.success(), "br close: {close:?}");
        match close_check(&dir, &bead_id).unwrap() {
            CloseCheck::Missing {
                sha,
                receipt_eligible,
            } => {
                assert!(
                    receipt_eligible,
                    "non-runnable VERIFY bead MAY close via WorkerReceipt"
                );
                assert!(!sha.is_empty());
            }
            other => panic!("expected Missing(receipt-eligible), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// governed-by: ADR-0019
// Wave-1 verdict spine (ADR-0019 Spec 1, amended 2026-08-15): the pure types
// and the legal-close matrix. No IO in this module by design (bd-w1-types-vbez).
// The br-gate verdict store, receipts helper, and loop-only reporter guard
// land in bd-w1-gate-store-h2ur on top of these types.
//
// The pub items here are the wave-1 spine: consumers (br gate wrappers, the
// `flywheel ledger` CLI, the loop close-detect path) land in the next beads.

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
    /// A named human attester (recorded as the gate `provider`) ordered the
    /// close at P0/P1, where no command can verify.
    OperatorDirected,
    /// Record-level evidence reviewed, no command applies (dupe/stale
    /// triage); legal in the P2+ non-runnable band, alongside `WorkerReceipt`.
    TriageVerified,
    /// Loop state, not a pass.
    VerifierBlocked,
    /// Loop state, not a pass.
    VerifierFailed,
}

impl VerdictKind {
    /// br gate name (kebab-cased `VerdictKind`, ADR-0019 Spec 1). Only PASS
    /// verdicts become gate rows; Blocked/Failed are loop state, but the names
    /// exist so the mapping is total. Every name br's transition policy
    /// enumerates (`beads::close_policy::ADR0019_VERDICT_GATE_NAMES`) is
    /// present — a name br offers that we cannot parse is a close the ledger
    /// can never credit (bd-tfxv).
    pub fn gate_name(self) -> &'static str {
        match self {
            Self::CommandVerified => "command-verified",
            Self::UnitTestVerified => "unit-test-verified",
            Self::LiveVerified => "live-verified",
            Self::ReviewerSigned => "reviewer-signed",
            Self::WorkerReceipt => "worker-receipt",
            Self::OperatorDirected => "operator-directed",
            Self::TriageVerified => "triage-verified",
            Self::VerifierBlocked => "verifier-blocked",
            Self::VerifierFailed => "verifier-failed",
        }
    }

    /// Inverse of `gate_name` — parse a br gate row's gate name back into a
    /// kind. Unknown names -> `None` (not a verdict row).
    pub fn from_gate_name(name: &str) -> Option<Self> {
        Some(match name {
            "command-verified" => Self::CommandVerified,
            "unit-test-verified" => Self::UnitTestVerified,
            "live-verified" => Self::LiveVerified,
            "reviewer-signed" => Self::ReviewerSigned,
            "worker-receipt" => Self::WorkerReceipt,
            "operator-directed" => Self::OperatorDirected,
            "triage-verified" => Self::TriageVerified,
            "verifier-blocked" => Self::VerifierBlocked,
            "verifier-failed" => Self::VerifierFailed,
            _ => return None,
        })
    }
    /// Every kind; the exhaustive-match surface for `legal_close`. The order
    /// matches br's `verify::VerdictKind::ALL` so a drift in either table is
    /// visible side by side (bd-tfxv: one gate-name authority).
    #[cfg(test)]
    pub const ALL: [Self; 9] = [
        Self::CommandVerified,
        Self::UnitTestVerified,
        Self::LiveVerified,
        Self::ReviewerSigned,
        Self::WorkerReceipt,
        Self::OperatorDirected,
        Self::TriageVerified,
        Self::VerifierBlocked,
        Self::VerifierFailed,
    ];
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

/// Parse the bead's TIMEBOX from its description (brief template:
/// `TIMEBOX 90min/bead` or `TIMEBOX: 30`). Returns SECONDS. Absent or
/// malformed -> `None` (the caller applies its default).
pub fn parse_timebox(description: &str) -> Option<u64> {
    for line in description.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("TIMEBOX") else {
            continue;
        };
        let rest = rest.trim_start_matches(':').trim();
        let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(mins) = num.parse::<u64>()
            && mins > 0
        {
            return Some(mins * 60);
        }
    }
    None
}

/// TIMEBOX (seconds) for a bead, from `br show`'s description; `default`
/// when absent, malformed, or the lookup fails (best-effort — the loop must
/// never die on a missing TIMEBOX).
pub fn bead_timebox_secs(cwd: &Path, bead_id: &str, default: u64) -> u64 {
    let Ok(stdout) = run_br(cwd, &["show", bead_id, "--json"]) else {
        return default;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return default;
    };
    let desc = v
        .get(0)
        .and_then(|b| b.get("description"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    parse_timebox(desc).unwrap_or(default)
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

/// Legal-close table (ADR-0019 Spec 1, amended 2026-08-15; kinds aligned
/// with br's `verify::legal_close` by bd-tfxv):
///
/// | Condition | Legal kinds |
/// |---|---|
/// | priority >= 2, blast Normal, VERIFY loop-runnable | `CommandVerified` (loop-run) ONLY |
/// | priority >= 2, blast Normal, VERIFY not loop-runnable | `WorkerReceipt` (+ two-tick grace), `TriageVerified`, or `UnitTestVerified` / `LiveVerified` |
/// | priority <= 1, blast Normal | `OperatorDirected` (provider names the human), or `UnitTestVerified` / `LiveVerified` |
/// | blast High | `UnitTestVerified`, `LiveVerified` only |
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
        // Row 2 — non-runnable band: WorkerReceipt (+ two-tick grace),
        // TriageVerified (reviewed record evidence), or an independent
        // unit/live verification.
        VerdictKind::WorkerReceipt | VerdictKind::TriageVerified => {
            ac == AcShape::Checkable
                && bead.priority >= 2
                && bead.blast == Blast::Normal
                && !runnable
        }
        // Row 2b — ordered close: a human attester (recorded as provider) is
        // the independent verifier at P0/P1, where no command can verify.
        VerdictKind::OperatorDirected => {
            ac == AcShape::Checkable && bead.priority <= 1 && bead.blast == Blast::Normal
        }
        // Rows 2+3 — independent verification closes the non-runnable band
        // (P>=2, Normal, VERIFY not loop-runnable) as well as P0/P1 / High
        // blast. Illegal in the cheap band, where CommandVerified is
        // exclusive (ancestor table: toron ADR-0019 Spec 1;
        // bd-legal-close-row2-reconcile-2m34).
        VerdictKind::UnitTestVerified | VerdictKind::LiveVerified => {
            ac == AcShape::Checkable
                && ((bead.priority <= 1 || bead.blast == Blast::High)
                    || (bead.priority >= 2 && bead.blast == Blast::Normal && !runnable))
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

fn loop_armed() -> bool {
    LOOP_ARMED.load(std::sync::atomic::Ordering::SeqCst)
}

/// One gate row from `br gate list <id> --json` history.
///
/// `note` is optional: worker-recorded rows carry no `note` key (br only
/// writes it when `--note` is passed, bd-u5yr). A note-less passed row
/// parses but can never satisfy the SHA rule below — absence of binding
/// is a skip, never a legal verdict.
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
    pub note: Option<String>,
    pub recorded_by: String,
    pub recorded_at: String,
}

/// `br gate list <id> --json` (unknown fields — legacy_results,
/// gated_transitions, … — are ignored by serde). Only `history` is read; the
/// gate path never needs the row's own id/status, and dropping the two
/// unread fields makes the deserializer strictly more permissive.
#[derive(Clone, Debug, Deserialize)]
pub struct GateList {
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

/// Paths that are tracker/orchestrator BOOKKEEPING, never work (ADR-0006 D3
/// dirt policy names the same two). A commit touching only these is not a
/// work revision — see [`commit_is_bookkeeping_only`].
const BOOKKEEPING_PATH_PREFIXES: [&str; 2] = [".beads/", ".flywheel/"];

/// True when `sha`'s changed paths are non-empty AND all bookkeeping.
///
/// An empty path list is NOT bookkeeping: a merge commit prints no paths
/// (no `-m`) and an empty commit has none — skipping on absence would
/// silently retarget the bound revision, which is the bug this guards.
fn commit_is_bookkeeping_only(cwd: &Path, sha: &str) -> anyhow::Result<bool> {
    let out = StdCommand::new("git")
        .args(["show", "--name-only", "--format=", "--no-renames", sha])
        .current_dir(cwd)
        .output()
        .map_err(|e| anyhow::anyhow!("git not found: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git show {sha} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let paths: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if paths.is_empty() {
        return Ok(false);
    }
    Ok(paths.iter().all(|p| {
        BOOKKEEPING_PATH_PREFIXES
            .iter()
            .any(|pre| p.starts_with(pre))
    }))
}

/// SHA rule (ADR-0019 Spec 1): the verdict must bind to the commit whose
/// message carries the bead id. `git log --all --grep=<id> --format=%H`,
/// no bare-HEAD fallback: no commit carries the id -> the close is illegal
/// regardless of verdicts.
///
/// Candidates are walked newest-first and a BOOKKEEPING-ONLY commit is
/// skipped. Why: the close ceremony itself writes `.beads/*`, and that
/// bookkeeping commit routinely cites the bead id — so the newest `--grep`
/// match became the bookkeeping commit rather than the revision the gate row
/// was bound to, and the row you had just bound read clean-false (unrepairable
/// once the bead is closed). The bound revision is the newest commit that
/// carries the id AND carries work.
///
/// Candidates are ranked with the SUBJECT before a body mention (bd-y98h).
/// A work commit carries the id in its subject (bracketed in the subject line);
/// the pre-commit guard's citation duty is what puts a SIBLING lane's id in the
/// BODY, because a hunk beside that lane's work must cite their bead id to
/// commit at all. Live 2026-09-28: a lane's body said "file_host.rs is reserved
/// CopperWren/bd-xkte and carries their in-flight work, so the call site is not
/// mine to add" one minute after the work commit, became the newest candidate,
/// and `ledger check` demanded a verdict at THEIR sha for MY bead — a reopen of
/// finished, gated work. Preferring the subject keeps every honest close bound
/// where it was; the body mention is only consulted when NO commit carries the
/// id in its subject.
///
/// Fail-closed: when EVERY candidate is bookkeeping-only, the newest one is
/// still returned — the id is attributable and "no commit carries the id"
/// would be a lie that reopens a legal close.
/// bd-pi4p: binding rank over parsed `(sha, subject-carries-id,
/// subject-carries-bracketed-id)` rows. Pure so both occurrence shapes are
/// unit-testable without git. Tiers, newest-first inside each: bracketed
/// subject citation, bare subject mention, body-only fallback.
/// Rank the candidates by citation tier.
///
/// The tiers ORDER the candidates, strongest first: a bracketed subject
/// citation, then a bare subject mention, then a body-only
/// mention, newest first inside a tier.
///
/// The tiers EXCLUDE the lower ones when a stronger tier is present - that is
/// what keeps a prose thief from moving the binding off the commit that
/// carries the work. The caller therefore filters the bookkeeping-only
/// commits out of the candidate set BEFORE calling this: a bookkeeping
/// commit that cites the id in its subject would otherwise hide the work
/// commit that cites it in its body, and the bound revision would become the
/// ceremony's own bookkeeping commit.
fn rank_binding<'a>(parsed: &'a [(String, bool, bool)], candidates: &[&'a str]) -> Vec<&'a str> {
    let cited: Vec<&str> = parsed
        .iter()
        .filter(|(sha, _, bracket)| *bracket && candidates.contains(&sha.as_str()))
        .map(|(sha, _, _)| sha.as_str())
        .collect();
    if !cited.is_empty() {
        return cited;
    }
    let mentioned: Vec<&str> = parsed
        .iter()
        .filter(|(sha, subject, _)| *subject && candidates.contains(&sha.as_str()))
        .map(|(sha, _, _)| sha.as_str())
        .collect();
    if !mentioned.is_empty() {
        return mentioned;
    }
    candidates.to_vec()
}

pub fn derive_head_sha(cwd: &Path, bead_id: &str) -> anyhow::Result<String> {
    // The subject rides along so the ranking is decided from ONE git call.
    // bd-ozpj: the grep is CASE-INSENSITIVE, like beads' own
    // `sha_message_cites_bead`. The case-sensitive grep made a commit that
    // cites BD-2QU9 invisible to this rule while closing legally in beads —
    // and a closed bead cannot be re-gated, so the ledger check was stuck
    // clean-false with no way out.
    let out = StdCommand::new("git")
        .args([
            "log",
            "--all",
            "--regexp-ignore-case",
            &format!("--grep={bead_id}"),
            "--format=%H%x1f%s",
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let wanted = bead_id.to_ascii_lowercase();
    let parsed: Vec<(String, bool, bool)> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| match l.split_once('\u{1f}') {
            Some((sha, subject)) => {
                let lower = subject.to_ascii_lowercase();
                (
                    sha.to_string(),
                    lower.contains(&wanted),
                    // bd-pi4p: structural citation `[bd-xxxx]` outranks a bare
                    // prose mention, so a later sentence can no longer steal the
                    // binding from the commit that cites it. bd-ozpj: the
                    // ranking reads the subject case-insensitively, like the
                    // grep that found it.
                    lower.contains(&format!("[{wanted}]")),
                )
            }
            None => (l.to_string(), false, false),
        })
        .collect();
    if parsed.is_empty() {
        anyhow::bail!(
            "SHA rule: no commit carries the bead id {bead_id} (git log --all --grep) — \
             close is illegal regardless of verdicts"
        );
    }
    // An EMPTY commit is not a candidate: it changes no files, so it cannot
    // contain the work. It satisfies the message grep and nothing else, and
    // `commit_is_bookkeeping_only` answers "not bookkeeping" for it, so on its
    // own it used to be handed out as the bound revision — a LEGAL close for a
    // commit that proves nothing. Refuse instead of certifying.
    let candidates: Vec<&str> = parsed
        .iter()
        .map(|(sha, _, _)| sha.as_str())
        .filter(|sha| !commit_changes_nothing(cwd, sha).unwrap_or(false))
        .collect();
    if candidates.is_empty() {
        anyhow::bail!(
            "SHA rule: every commit carrying bead id {bead_id} is EMPTY (it changes no files), \
             so none of them can prove it — close against the commit that carries the work"
        );
    }
    // Ranked (bd-pi4p tiers): bracketed-subject citations first, bare
    // subject mentions next, body-only fallback last — newest-first inside
    // each tier. Every close this crew ships cites `[bd-xxxx]` in the
    // subject, so tier 1 resolves exactly as before; only prose thieves
    // move down.
    //
    // The bookkeeping-only commits are dropped BEFORE the ranking, not after
    // it: the ranking returns only the strongest citation tier,
    // so a bookkeeping commit that cites the id in its SUBJECT - which is
    // exactly what `chore(beads): close <id>` does - used to hide the work
    // commit that cites it in its BODY. The bound revision then became the
    // ceremony's own bookkeeping commit: a LEGAL close that proves nothing,
    // because that commit carries no work. Measured on the toron repo:
    // two beads were both bound to a `chore(beads)` commit.
    let mut work_candidates: Vec<&str> = Vec::new();
    let mut bookkeeping_candidates: Vec<&str> = Vec::new();
    for sha in &candidates {
        if commit_is_bookkeeping_only(cwd, sha)? {
            bookkeeping_candidates.push(sha);
        } else {
            work_candidates.push(sha);
        }
    }
    if work_candidates.is_empty() {
        // Every candidate is bookkeeping-only: the id is still attributable
        // and "no commit carries the id" would be a lie that reopens a legal
        // close. Bind the strongest citation among them.
        let ranked = rank_binding(&parsed, &bookkeeping_candidates);
        return Ok(ranked
            .first()
            .copied()
            .unwrap_or_else(|| candidates.first().copied().unwrap_or_default())
            .to_string());
    }
    let ranked = rank_binding(&parsed, &work_candidates);
    Ok(ranked
        .first()
        .copied()
        .unwrap_or_else(|| work_candidates.first().copied().unwrap_or_default())
        .to_string())
}

/// Git stdout for `args` in `cwd`, trimmed of the trailing newline.
fn git_stdout(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = StdCommand::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| anyhow::anyhow!("git not found: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// True when `sha` changes no files at all: an empty commit.
///
/// A merge is never called empty here — `git show --name-only` prints nothing
/// for a merge without `-m`, so the answer would be a false positive.
fn commit_changes_nothing(cwd: &Path, sha: &str) -> anyhow::Result<bool> {
    let parents = git_stdout(cwd, &["rev-list", "--parents", "-n", "1", sha])?;
    if parents.split_whitespace().count() > 2 {
        return Ok(false);
    }
    Ok(git_stdout(
        cwd,
        &["show", "--name-only", "--format=", "--no-renames", sha],
    )?
    .is_empty())
}

/// Rebuild a `BeadUnit` from `br show <id> --json`: priority, title, and the
/// typed `verify` field (ADR-0001 wave 5, bd-wave5-flywheel-parser-strip-dkpvi —
/// the description fence is no longer read). Blast/AC-shape are not in br; the
/// caller supplies them (the loop derives blast from reservation overlap + AC
/// shape at ready-parse).
pub fn bead_from_br_show(cwd: &Path, bead_id: &str) -> anyhow::Result<BeadUnit> {
    load_bead(cwd, bead_id, /* require_verify */ true)
}

/// Close-crediting loader: identical to [`bead_from_br_show`] except that a
/// bead with NO VERIFY is admissible.
///
/// Why this exists: `require_verify` is a DISPATCH precondition (nothing to
/// run -> do not hand the bead to a worker), not a close precondition. A
/// triage/dupe bead has no VERIFY by definition, and its legal proof is a
/// record review (`triage-verified`) rather than a command. Applying the
/// dispatch precondition while crediting a close made br's own refusal list
/// uncreditable for exactly the bead class it names (bd-tfxv).
///
/// The empty VERIFY is safe here because `verify_is_loop_runnable("")` is
/// false, so the bead lands in the non-runnable band where the legal-close
/// table admits only record/independent proofs — never `CommandVerified`.
pub fn bead_from_br_show_for_close(cwd: &Path, bead_id: &str) -> anyhow::Result<BeadUnit> {
    load_bead(cwd, bead_id, /* require_verify */ false)
}

fn load_bead(cwd: &Path, bead_id: &str, require_verify: bool) -> anyhow::Result<BeadUnit> {
    #[derive(Deserialize)]
    struct BrShow {
        id: String,
        title: String,
        priority: u8,
        verify: Option<String>,
    }
    let stdout = run_br(cwd, &["show", bead_id, "--json"])?;
    let v: serde_json::Value = serde_json::from_str(stdout.trim())?;
    // br show <id> --json historically returned a one-element array; current
    // br returns the object directly. Accept both shapes (bd-repair-red-tests:
    // the array expectation made EVERY close_check fail to deserialize and
    // reopen legally-closed beads).
    let row_val = if v.is_array() {
        v.get(0).cloned().unwrap_or(serde_json::Value::Null)
    } else {
        v
    };
    let row: BrShow = serde_json::from_value(row_val)?;
    let verify = row.verify.filter(|s| !s.trim().is_empty());
    let verify = match (verify, require_verify) {
        (Some(v), _) => v,
        (None, false) => String::new(),
        (None, true) => {
            anyhow::bail!("bead {bead_id}: missing/empty typed verify field — refuse to dispatch")
        }
    };
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
/// A note is a LEADING run of `key=value` pairs. Parsing stops at the first
/// token that is not one, so prose can never contribute a binding token
/// (bd-0z00): `supersedes sha=<old> because the work is at sha=<new>` binds
/// nothing, instead of silently binding whichever revision the prose named
/// first. `report_pass` writes `sha=<sha> receipt=<path>`, so the ceremony's
/// own note is a two-pair run and stays legal.
fn note_tokens(note: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for tok in note.split_whitespace() {
        match tok.split_once('=') {
            Some((k, v)) if !k.is_empty() => out.push((k, v)),
            _ => break,
        }
    }
    out
}

/// A full git object name: exactly 40 hex digits. A short prefix is not a
/// binding, because `derive_head_sha` resolves full shas only.
fn is_full_sha(v: &str) -> bool {
    v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What a note binds to, and why not when it binds nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoteBinding {
    /// The note leads with `sha=<40 hex>`: the verdict binds here.
    Bound(String),
    /// The note leads with prose, or with a key that is not `sha`.
    NoLeadingBinding,
    /// The note leads with `sha=` but the value is not a full 40-hex sha.
    /// A trailing comma lands here: the comma is captured with the value.
    MalformedSha(String),
}

pub(crate) fn note_binding(note: &str) -> NoteBinding {
    match note_tokens(note).first() {
        None => NoteBinding::NoLeadingBinding,
        Some((k, _)) if *k != "sha" => NoteBinding::NoLeadingBinding,
        Some((_, v)) if !is_full_sha(v) => NoteBinding::MalformedSha((*v).to_string()),
        Some((_, v)) => NoteBinding::Bound((*v).to_string()),
    }
}

#[cfg(test)]
mod binding_rank_tests {
    use super::*;

    fn row(sha: &str, subject: bool, bracket: bool) -> (String, bool, bool) {
        (sha.to_string(), subject, bracket)
    }

    /// bd-pi4p occurrence 1: thief mentions the id in body prose (newest),
    /// real commit cites it bracketed in the subject. Citation wins.
    #[test]
    fn bracketed_subject_beats_newer_body_prose() {
        let parsed = vec![row("thief", false, false), row("real", true, true)];
        let cands = vec!["thief", "real"];
        assert_eq!(rank_binding(&parsed, &cands), vec!["real"]);
    }

    /// bd-pi4p hardening: thief carries a BARE id in its subject
    /// ("follow-up to bd-1"), real commit cites bracketed. Citation wins.
    #[test]
    fn bracketed_subject_beats_bare_subject_mention() {
        let parsed = vec![row("thief", true, false), row("real", true, true)];
        let cands = vec!["thief", "real"];
        assert_eq!(rank_binding(&parsed, &cands), vec!["real"]);
    }

    /// Pre-marker shape preserved: no brackets anywhere, bare subject
    /// mention still outranks body-only.
    #[test]
    fn bare_subject_still_beats_body_without_brackets() {
        let parsed = vec![row("thief", false, false), row("real", true, false)];
        let cands = vec!["thief", "real"];
        assert_eq!(rank_binding(&parsed, &cands), vec!["real"]);
    }

    /// All body-only: newest-first fallback, unchanged.
    #[test]
    fn body_only_falls_back_to_newest_first() {
        let parsed = vec![row("new", false, false), row("old", false, false)];
        let cands = vec!["new", "old"];
        assert_eq!(rank_binding(&parsed, &cands), vec!["new", "old"]);
    }
}

#[cfg(test)]
mod note_binding_tests {
    use super::*;

    /// MUTATION 1: restore the prose scan `note_tokens` replaced. The
    /// prose-mention test must fail, proving it detects the regression.
    #[test]
    fn mutation_prose_scan_is_caught() {
        fn unanchored(note: &str) -> Option<&str> {
            note.split_whitespace()
                .find_map(|tok| tok.strip_prefix("sha")?.strip_prefix('='))
        }
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let note = format!("supersedes sha={a} because the new work is at sha={b}");
        // The old behavior: the first prose mention wins and binds.
        assert_eq!(unanchored(&note), Some(a.as_str()));
        // The fixed behavior refuses it.
        assert_eq!(note_binding(&note), NoteBinding::NoLeadingBinding);
        assert!(!note_has_sha(&note, &a));
    }

    /// MUTATION 2: drop the 40-hex check. The comma test must fail.
    #[test]
    fn mutation_missing_hex_length_check_is_caught() {
        fn unvalidated(note: &str) -> Option<&str> {
            note.split_whitespace()
                .find_map(|tok| tok.strip_prefix("sha="))
        }
        let sha = "c".repeat(40);
        let note = format!("sha={sha},");
        // Without the length/hex check the comma rides along and matches a
        // prefix comparison, which is exactly the silent mis-bind.
        assert_eq!(unvalidated(&note), Some(format!("{sha},").as_str()));
        // The real validator rejects it.
        assert!(matches!(note_binding(&note), NoteBinding::MalformedSha(_)));
        assert!(!note_has_sha(&note, &sha));
    }

    #[test]
    fn note_proved_reads_only_a_leading_pair() {
        assert_eq!(
            note_proved("task=run-1 verdict=v-9 receipt=/tmp/r.txt"),
            Some(("run-1".to_string(), "v-9".to_string()))
        );
        assert_eq!(note_proved("proved task=run-1 verdict=v-9"), None);
        assert_eq!(note_proved("task=run-1, verdict=v-9"), None);
        let sha = "a".repeat(40);
        assert_eq!(
            note_proved(&format!("sha={sha} task=run-1 verdict=v-9")),
            None
        );
    }
}

fn note_has_sha(note: &str, sha: &str) -> bool {
    matches!(note_binding(note), NoteBinding::Bound(v) if v == sha)
}

/// Phase 5 proved binding. The leading pair must be `task=<id> verdict=<id>`.
/// A later pair, a prose mention, or a comma glued to a value does not bind.
pub(crate) fn note_proved(note: &str) -> Option<(String, String)> {
    let tokens = note_tokens(note);
    let (k0, v0) = tokens.first().copied()?;
    let (k1, v1) = tokens.get(1).copied()?;
    if k0 != "task" || k1 != "verdict" {
        return None;
    }
    if !is_binding_id(v0) || !is_binding_id(v1) {
        return None;
    }
    Some((v0.to_string(), v1.to_string()))
}

fn is_binding_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'@'))
}

fn note_receipt_path(note: &str) -> Option<PathBuf> {
    note_tokens(note)
        .into_iter()
        .find(|(k, _)| *k == "receipt")
        .map(|(_, v)| PathBuf::from(v))
}

/// Why `note` does not bind `sha`, naming every `sha=` token the note carries
/// so the author sees the competing candidates instead of a bare "no legal
/// verdict" (bd-0z00). `None` when the note does bind `sha`.
pub(crate) fn note_binding_reason(note: &str, sha: &str) -> Option<String> {
    if note_has_sha(note, sha) {
        return None;
    }
    let candidates: Vec<&str> = note
        .split_whitespace()
        .filter(|t| t.starts_with("sha="))
        .collect();
    let found = if candidates.is_empty() {
        "no sha= token in the note".to_string()
    } else {
        format!("found {}", candidates.join(", "))
    };
    let why = match note_binding(note) {
        NoteBinding::Bound(v) => format!("note binds sha {v}, not the revision {sha}"),
        NoteBinding::MalformedSha(v) => {
            format!("the note's leading sha= value is not a full 40-hex sha (got {v:?})")
        }
        NoteBinding::NoLeadingBinding => {
            "the note does not lead with a sha=<40-hex> binding token".to_string()
        }
    };
    Some(format!("{why}; {found}"))
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

/// ADR-0006 VERIFY-stage piece (bd-principles-receipts-mmzc): the reusable
/// close-reason check shared by `flywheel ledger receipt --close-reason` and
/// the wave-swarm VERIFY stage. Pure — no IO, fixture-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReasonCheck {
    /// The close reason carries the VERIFY tail (what was actually run).
    pub verify_tail_present: bool,
    /// Well-formed `name — decision` citations parsed from the close
    /// reason's PRINCIPLES section.
    pub citations: Vec<(String, String)>,
    /// Lint-level warnings: unknown principle names, cargo-cult decoration
    /// (registry names dropped in prose without a decision clause), and
    /// citation-count drift outside 1-3. Advisory until proven — nothing
    /// here hard-fails a close yet (ADR-0006 §3 D3).
    pub lint_warnings: Vec<String>,
}

impl CloseReasonCheck {
    /// The advisory stance: decoration detection is a lint warning, not a
    /// gate, until proven in practice (ADR-0006 D3 VERIFY stage).
    #[cfg(test)]
    pub fn is_hard_fail(&self) -> bool {
        false
    }
}

/// Check a bead close reason for its VERIFY tail and principles citations:
///
/// - VERIFY tail: the reason must name what was actually run (`VERIFY`
///   marker present).
/// - Citations live in a `PRINCIPLES:` section; each line/clause must be
///   `name — decision it changed` (em-dash, matching the report checker in
///   [`crate::principles::check_principles`]); 1-3 citations expected.
/// - Every cited name must exist in the canonical 21-name registry.
/// - A registry name appearing OUTSIDE a well-formed citation is a
///   decoration (cargo-cult prose) — flagged, not failed.
pub fn check_close_reason(reason: &str) -> CloseReasonCheck {
    let mut out = CloseReasonCheck {
        verify_tail_present: reason.contains("VERIFY"),
        citations: Vec::new(),
        lint_warnings: Vec::new(),
    };
    let (principles_text, prose) = match reason.find("PRINCIPLES") {
        Some(i) => (&reason[i + "PRINCIPLES".len()..], &reason[..i]),
        None => ("", reason),
    };
    // Clauses are newline- or semicolon-separated citation candidates.
    for clause in principles_text.split(['\n', ';']) {
        let clause = clause.trim().trim_start_matches(':').trim();
        if clause.is_empty() {
            continue;
        }
        if !clause.contains('—') && !clause.contains("—") {
            // No separator at all: not a citation; the decoration scan below
            // decides whether registry names were dropped in prose.
            continue;
        }
        let Some((name, decision)) = clause
            .split_once('—')
            .or_else(|| clause.split_once('\u{2014}'))
        else {
            continue;
        };
        let name = name.trim();
        let decision = decision.trim();
        if decision.is_empty() {
            out.lint_warnings.push(format!(
                "citation has no decision clause (decoration?): {clause:?}"
            ));
            continue;
        }
        out.citations.push((name.to_string(), decision.to_string()));
        if !crate::principles::is_known_name(name) {
            out.lint_warnings.push(format!(
                "unknown principle name {name:?} in citation (canonical 21, see `flywheel principles list`)"
            ));
        }
    }
    if out.citations.is_empty() || out.citations.len() > 3 {
        out.lint_warnings.push(format!(
            "close reason carries {} principles citations, expected 1-3",
            out.citations.len()
        ));
    }
    // Decoration scan: registry names dropped in prose outside the
    // PRINCIPLES section are cargo-cult mentions, not citations.
    for p in crate::principles::PRINCIPLES {
        if prose.contains(p.name) {
            out.lint_warnings.push(format!(
                "decoration: principle `{}` named in prose without a `name — decision` citation",
                p.name
            ));
        }
    }
    out
}

/// ADR-0006 D3 (bd-wave-swarm-recipe-wsot): the wave-swarm VERIFY stage's
/// HARD close-reason gate. `check_close_reason` stays advisory (receipt
/// writes never fail on lint); the composed VERIFY stage needs a verdict —
/// every advisory warning is a reject here. Pure; the fixture surface the
/// graduation bar tests.
pub fn verify_close_reason(reason: &str) -> Result<(), String> {
    let c = check_close_reason(reason);
    if !c.verify_tail_present {
        return Err("close reason carries no VERIFY tail (what was actually run)".to_string());
    }
    // A bare name with an em-dash but NO decision clause is named first —
    // it is a citation attempt, not an absence.
    if let Some(w) = c
        .lint_warnings
        .iter()
        .find(|w| w.contains("no decision clause"))
    {
        return Err(w.clone());
    }
    if c.citations.is_empty() {
        return Err(
            "close reason cites no principles (`<known-name> — <decision>` required)".to_string(),
        );
    }
    if !c.lint_warnings.is_empty() {
        return Err(c.lint_warnings.join("; "));
    }
    Ok(())
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
        // `sha=<sha>`. A Phase 5 `task=` `verdict=` pair is a separate hit
        // (`ledger_proved`); it does not satisfy the SHA rule.
        if !row.note.as_deref().is_some_and(|n| note_has_sha(n, sha)) {
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
            receipt_path: row
                .note
                .as_deref()
                .and_then(note_receipt_path)
                .unwrap_or_default(),
            verify_cmd: bead.verify.clone(),
        };
        if legal_close(&verdict, bead, ac) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A PASS row whose note leads with `task=<id> verdict=<id>` and whose kind
/// is legal for this bead. The SHA rule is not consulted.
pub fn ledger_proved(
    cwd: &Path,
    bead: &BeadUnit,
    ac: AcShape,
) -> anyhow::Result<Option<(String, String)>> {
    let list = list_gates(cwd, &bead.id)?;
    for row in &list.history {
        if !row.passed {
            continue;
        }
        let Some(kind) = VerdictKind::from_gate_name(&row.gate) else {
            continue;
        };
        if matches!(
            kind,
            VerdictKind::VerifierBlocked | VerdictKind::VerifierFailed
        ) {
            continue;
        }
        let Some(note) = row.note.as_deref() else {
            continue;
        };
        let Some((task, verdict)) = note_proved(note) else {
            continue;
        };
        let Some(at) = parse_recorded_at(&row.recorded_at) else {
            continue;
        };
        let verdict_row = Verdict {
            kind,
            bead_id: bead.id.clone(),
            sha: String::new(),
            at,
            actor: AgentName::new(row.provider.clone()),
            receipt_path: note_receipt_path(note).unwrap_or_default(),
            verify_cmd: bead.verify.clone(),
        };
        if legal_close(&verdict_row, bead, ac) {
            return Ok(Some((task, verdict)));
        }
    }
    Ok(None)
}

/// Why no gate row legalized `sha`: one line per pass row whose note carries a
/// `sha=` token that does not bind. `flywheel ledger check` reports this on
/// the failure path so an author sees the competing candidates instead of a
/// bare "no legal verdict" (bd-0z00). Only called when `ledger_check` is
/// false, so a legal check pays nothing for it.
pub(crate) fn ledger_check_diagnostics(
    cwd: &Path,
    bead: &BeadUnit,
    sha: &str,
) -> anyhow::Result<Vec<String>> {
    let list = list_gates(cwd, &bead.id)?;
    let mut out = Vec::new();
    for row in &list.history {
        if !row.passed || VerdictKind::from_gate_name(&row.gate).is_none() {
            continue;
        }
        let Some(note) = row.note.as_deref() else {
            continue;
        };
        if let Some(why) = note_binding_reason(note, sha) {
            out.push(format!("gate {}: {why}", row.gate));
        }
    }
    Ok(out)
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
/// kind (anti-forgery by construction, ADR-0019 Spec 1). Unlike a worker
/// receipt, content may be empty — a silent pass is still evidence of the
/// loop's own execution; the armed gate is the proof, not the bytes.
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

/// Serializes tests that arm the loop reporter: arming is process-global, so
/// a parallel armed test would race another test's unarmed-refusal
/// assertions. Tests take this lock for the whole arming sequence.
#[cfg(test)]
#[doc(hidden)]
pub static LOOP_REPORTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`LOOP_REPORTER_LOCK`] (poison-tolerant). Test-only.
///
/// Mirrors `config::lock_env`: a `Mutex` stays poisoned for the life of the
/// process once a holder panics, so a plain `.lock().unwrap()` would turn one
/// unrelated panicking test into a permanent failure for every later test that
/// arms the loop reporter. `LOOP_ARMED` is process-global and persists across
/// tests, so the arming sequence runs many times per binary and a poisoned lock
/// here is very costly.
#[cfg(test)]
#[doc(hidden)]
pub fn lock_loop_reporter() -> std::sync::MutexGuard<'static, ()> {
    LOOP_REPORTER_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Arm the loop reporter. Called once by the durable loop at startup
/// (durable.rs, bd-w1-reopen-f6tw wiring). Idempotent.
#[cfg(test)]
pub fn arm_loop_reporter() {
    LOOP_ARMED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Test seam: reset the process-global arm flag (call under
/// `LOOP_REPORTER_LOCK` at the start of an arming test — the flag persists
/// across tests once armed).
#[cfg(test)]
#[doc(hidden)]
pub fn test_reset_loop_reporter() {
    LOOP_ARMED.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;

    use super::*;

    /// bd-ozpj: the citation lookup is CASE-INSENSITIVE, like beads' own
    /// `sha_message_cites_bead`.
    ///
    /// The case-sensitive grep made a commit that cites `BD-2QU9` invisible
    /// to this rule while it closed legally in beads — and a closed bead
    /// cannot be re-gated, so the ledger check was stuck clean-false with
    /// no way out.
    #[test]
    fn citation_lookup_is_case_insensitive() {
        let dir = std::env::temp_dir().join(format!("fw-cite-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "fix: cite BD-2QU9 in the subject"]);
        let head = git_stdout(&dir, &["log", "-1", "--format=%H"]).expect("head");
        let bound = derive_head_sha(&dir, "bd-2qu9")
            .expect("the lowercase bead id must find the uppercase citation");
        assert_eq!(bound, head, "the uppercase citation must bind the bead");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            // Row 2 — non-runnable band: WorkerReceipt, TriageVerified, or
            // unit/live verified.
            WorkerReceipt | TriageVerified => {
                ac == Checkable && priority >= 2 && blast == Normal && !runnable
            }
            // Row 2b — ordered close: a named human attester is the
            // independent verifier at P0/P1 where no command can verify.
            OperatorDirected => ac == Checkable && priority <= 1 && blast == Normal,
            // Row 3 — P0/P1 or High blast: independent verification only
            // (row 2's non-runnable band also admits unit/live — bd-legal-
            // close-row2-reconcile-2m34).
            UnitTestVerified | LiveVerified => {
                ac == Checkable
                    && ((priority <= 1 || blast == High)
                        || (priority >= 2 && blast == Normal && !runnable))
            }
        }
    }

    /// Acceptance: table test covers EVERY kind x priority(0..4) x blast x
    /// AC-shape x loop-runnability cell (9 * 5 * 2 * 2 * 2 = 360 cells).
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

    // ---- bd-w1-loop-run-verify-mbbe: loop-runnable classifier ----

    /// One row per input of the loop-runnable allowlist predicate. Targeted,
    /// allowlisted checks (incl. a bounded `timeout` wrapper) pass;
    /// whole-workspace matrices, unbounded shells, non-verification commands,
    /// shell composition and empty VERIFY never do.
    #[test]
    fn loop_run_verify_classifier_row_table() {
        let accepts = [
            "cargo test -p flywheel verdict::",
            "cargo test -p flywheel loop_run_verify",
            "cargo test --package flywheel verifier",
            "pytest tests/test_gate.py",
            "python -m pytest tests/test_x.py",
            "timeout 300 cargo test -p flywheel verifier",
            "sh -c \"timeout 60 cargo test -p flywheel verifier\"",
            "test -f Cargo.toml",
        ];
        let rejects = [
            "",
            "   ",
            "cargo test --workspace",
            "cargo test --all-features",
            "cargo test --all-targets",
            "cargo test",
            "cargo test --lib",
            "cargo build",
            "bash scripts/verify-gates.sh",
            "sh -c 'cargo test --workspace'",
            "sleep 3600",
            "git push",
            "npm run e2e",
            "cargo test -p flywheel verifier && echo done",
            "cargo test && sleep 3600",
            "cargo test || cargo build",
            "cargo test | tee out.txt",
            "cargo test; echo done",
            "cargo test\ncargo build",
            "cargo test `echo x`",
            "cargo test $(echo x)",
        ];
        for cmd in accepts {
            assert!(
                verify_is_loop_runnable(cmd),
                "must be loop-runnable: {cmd:?}"
            );
        }
        for cmd in rejects {
            assert!(
                !verify_is_loop_runnable(cmd),
                "must NOT be loop-runnable: {cmd:?}"
            );
        }
    }

    /// TIMEBOX parse: minutes -> seconds; absent/malformed -> None.
    #[test]
    fn loop_run_verify_parses_timebox_to_seconds() {
        assert_eq!(parse_timebox("TIMEBOX 90min/bead"), Some(5400));
        assert_eq!(parse_timebox("TIMEBOX: 30"), Some(1800));
        assert_eq!(parse_timebox("TIMEBOX 45 minutes"), Some(2700));
        assert_eq!(parse_timebox("no timebox here"), None);
        assert_eq!(parse_timebox("TIMEBOX"), None);
        assert_eq!(parse_timebox("TIMEBOX 0"), None);
    }

    /// Anti-forgery: the WORKER receipt helper refuses CommandVerified
    /// outright (no arm dependency — always refused).
    #[test]
    fn worker_ledger_receipt_refuses_command_verified_kind() {
        let dir = std::env::temp_dir().join(format!("lr-receipt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err =
            write_receipt(&dir, "bd-x", "abc", VerdictKind::CommandVerified, "out").unwrap_err();
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
            assert_ne!(k.gate_name(), "");
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
        scratch_workspace_with_verify(tag, Some("cargo test -p flywheel verdict::"))
    }

    /// [`scratch_workspace`] with control over the bead's typed VERIFY —
    /// `None` models a triage/dupe bead, whose legal close is a record review
    /// and not a command (bd-tfxv).
    fn scratch_workspace_with_verify(tag: &str, verify: Option<&str>) -> (PathBuf, String) {
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
        // No VERIFY (triage/dupe shape): fail-closed `br create` refuses
        // without --verify at every priority, so use the sanctioned
        // no-verify path `br q` (quick capture, not dispatchable until
        // verify is added) — NOT -p 4 (verify is still required there)
        // and NOT a weakened assertion. P3 + empty VERIFY preserved.
        // (`br q` takes a bare title + -p/-d only: no --title/--slug.)
        let mut args: Vec<&str> = match verify {
            Some(_) => vec![
                "create",
                "--title",
                "gate roundtrip",
                "--slug",
                "gate-rt-1",
                "-p",
                "3",
                "-d",
                "work",
            ],
            None => vec!["q", "gate roundtrip", "-p", "3", "-d", "work"],
        };
        if let Some(v) = verify {
            args.extend(["--verify", v]);
        }
        let created = StdCommand::new("br")
            .args(&args)
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

    /// An empty commit can cite the bead id and change nothing.
    ///
    /// The message grep finds it, and `commit_is_bookkeeping_only` answers
    /// "not bookkeeping" for it (an empty path list is deliberately not
    /// bookkeeping, so a missing path list can never silently retarget the
    /// bound revision) — which together made `git commit --allow-empty -m
    /// "<id>"` a free LEGAL close for a bead nobody worked (2026-09-28: a
    /// concurrent `git reset` unstaged the index and the commit landed empty).
    #[test]
    fn derive_head_sha_skips_an_empty_commit() {
        let (dir, bead_id) = scratch_workspace("sha-empty-skip");
        let work = derive_head_sha(&dir, &bead_id).unwrap();
        let out = std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", &bead_id])
            .current_dir(&dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "empty commit failed: {out:?}");
        assert_ne!(head_sha(&dir), work, "HEAD moved to the empty commit");
        assert_eq!(
            derive_head_sha(&dir, &bead_id).unwrap(),
            work,
            "an empty commit must never become the bound revision"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And when every commit carrying the id is empty, the answer is a refusal
    /// that names the empty commit — never a LEGAL verdict on a commit that
    /// cannot contain the work.
    #[test]
    fn derive_head_sha_refuses_when_every_candidate_is_empty() {
        let bead_id = "test-empty-only";
        let dir = std::env::temp_dir().join(format!("fw-verdict-emptyonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tempdir");
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", bead_id],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        }
        let err = derive_head_sha(&dir, bead_id).expect_err("an empty-only binding must refuse");
        assert!(
            err.to_string().to_lowercase().contains("empty"),
            "the refusal must name the empty commit, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Acceptance round-trip: `br gate report` then ledger check legal on the
    /// same bead — plus the loop-only reporter guard on both paths
    /// (worker-invoked report blocked, loop path accepted).
    #[test]
    fn loop_only_reporter_guard_blocks_workers_then_loop_roundtrips() {
        // Arming is process-global — serialize with other arming tests and
        // reset the flag (it persists once armed).
        let _lock = crate::verdict::lock_loop_reporter();
        crate::verdict::test_reset_loop_reporter();
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
        assert!(
            list.history[0]
                .note
                .as_deref()
                .is_some_and(|n| note_has_sha(n, &sha))
        );
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
            dir.join(format!(
                ".flywheel/receipts/{bead_id}-{sha}.command-verified.txt"
            ))
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

    // ---- bd-tfxv: one gate-name authority across br + flywheel ----

    /// Every gate name br's own transition policy enumerates
    /// (`beads::close_policy::ADR0019_VERDICT_GATE_NAMES`, literal copy) must
    /// parse into a flywheel `VerdictKind`. Before bd-tfxv, `operator-directed`
    /// and `triage-verified` did not — a worker following br's refusal message
    /// (`br gate report --gate triage-verified`) landed a row that
    /// `ledger_check` skipped forever, so the bead could never be credited.
    #[test]
    fn every_br_gate_name_parses_to_a_kind() {
        for gate in [
            "command-verified",
            "unit-test-verified",
            "live-verified",
            "reviewer-signed",
            "worker-receipt",
            "operator-directed",
            "triage-verified",
        ] {
            assert!(
                VerdictKind::from_gate_name(gate).is_some(),
                "br gate {gate:?} has no flywheel VerdictKind"
            );
        }
    }

    /// The other half of bd-tfxv: a bead with NO VERIFY is a triage/dupe bead.
    /// Its legal proof is a record review, not a command — so the CLOSE path
    /// must be able to load it, while the DISPATCH path keeps demanding a
    /// VERIFY. Before this, `flywheel ledger check` refused on
    /// `missing/empty typed verify field — refuse to dispatch` and the
    /// `triage-verified` row (the gate br itself names for this shape) could
    /// never be credited.
    #[test]
    fn triage_bead_without_verify_credits_a_triage_verified_row() {
        let (dir, bead_id) = scratch_workspace_with_verify("triage-close", None);
        // Dispatch still refuses: no VERIFY, nothing to run, do not dispatch.
        let err = bead_from_br_show(&dir, &bead_id).unwrap_err();
        assert!(err.to_string().contains("refuse to dispatch"), "{err}");
        // Close crediting loads it, and the empty VERIFY lands in the
        // non-runnable band (nothing could have been run).
        let bead = bead_from_br_show_for_close(&dir, &bead_id).unwrap();
        assert_eq!(bead.priority, 3);
        assert_eq!(bead.verify, "");
        assert!(!verify_is_loop_runnable(&bead.verify));
        let sha = derive_head_sha(&dir, &bead_id).unwrap();
        let v = |kind| Verdict {
            kind,
            bead_id: bead_id.clone(),
            sha: sha.clone(),
            at: Utc::now(),
            actor: AgentName::new("reviewer:QuietHeron"),
            receipt_path: PathBuf::new(),
            verify_cmd: String::new(),
        };
        for gate in ["triage-verified", "worker-receipt", "unit-test-verified"] {
            let kind = VerdictKind::from_gate_name(gate).unwrap();
            assert!(
                legal_close(&v(kind), &bead, AcShape::Checkable),
                "gate {gate:?} should credit a triage bead"
            );
        }
        // Nothing could have been run, so a loop-run proof is a false verdict.
        assert!(!legal_close(
            &v(VerdictKind::CommandVerified),
            &bead,
            AcShape::Checkable
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ...and the parsed row must actually CLOSE the bead, not merely parse:
    /// this is `ledger_check`'s body (from_gate_name -> legal_close) for the
    /// shape br told the operator was legal.
    #[test]
    fn ledger_check_credits_the_rows_br_names_for_a_p3_script_verify() {
        // bd-tfxv's live shape: P3, Normal blast, checkable AC, VERIFY is a
        // committed bash script (non-runnable: `sh`/`bash` demands an explicit
        // `timeout` token). br named exactly these gates as legal.
        let bead = bead(3, Blast::Normal, /* runnable */ false);
        for gate in ["worker-receipt", "triage-verified", "unit-test-verified"] {
            let kind = VerdictKind::from_gate_name(gate)
                .unwrap_or_else(|| panic!("br gate {gate:?} unparseable"));
            assert!(
                legal_close(&verdict(kind), &bead, AcShape::Checkable),
                "ledger_check would refuse br-legal gate {gate:?}"
            );
        }
    }

    #[test]
    fn triage_verified_closes_the_non_runnable_band_only() {
        // Row 2, alongside WorkerReceipt: p>=2 + Normal + VERIFY not runnable.
        let non_runnable = bead(3, Blast::Normal, false);
        let v = verdict(VerdictKind::TriageVerified);
        assert!(legal_close(&v, &non_runnable, AcShape::Checkable));
        // Illegal in the cheap band: a loop-run command existed, so a
        // record-review is the wrong proof (ADR-0019 Spec 1 row 1).
        let cheap = bead(3, Blast::Normal, true);
        assert!(!legal_close(&v, &cheap, AcShape::Checkable));
        // Judgment AC: ReviewerSigned only.
        assert!(!legal_close(&v, &non_runnable, AcShape::Judgment));
    }

    #[test]
    fn operator_directed_closes_p0_p1_normal_only() {
        // Row 2b: a named human attester is the independent verifier where no
        // command can satisfy the close (P0/P1, Normal blast, checkable AC).
        let v = verdict(VerdictKind::OperatorDirected);
        for priority in [0u8, 1] {
            assert!(legal_close(
                &v,
                &bead(priority, Blast::Normal, false),
                AcShape::Checkable
            ));
        }
        // Not a P>=2 bead (the loop/verifier bands own those).
        assert!(!legal_close(
            &v,
            &bead(3, Blast::Normal, false),
            AcShape::Checkable
        ));
        // Not High blast (row 3: independent verification only).
        assert!(!legal_close(
            &v,
            &bead(0, Blast::High, false),
            AcShape::Checkable
        ));
        // Not judgment AC (row 4: ReviewerSigned only).
        assert!(!legal_close(
            &v,
            &bead(0, Blast::Normal, false),
            AcShape::Judgment
        ));
    }

    #[test]
    fn note_key_value_tokens_parse_sha_and_receipt() {
        // bd-0z00: a binding is a FULL 40-hex object name, not a short
        // prefix, so this note uses a real-shaped sha.
        let sha = "d".repeat(40);
        let note = format!("sha={sha} receipt=/tmp/r.txt");
        assert!(note_has_sha(&note, &sha));
        assert!(!note_has_sha(&note, "beef"));
        assert_eq!(
            note_receipt_path(&note).unwrap(),
            PathBuf::from("/tmp/r.txt")
        );
        assert_eq!(note_receipt_path("no tokens"), None);
    }

    /// bd-0z00: a note binds at its START. Prose that mentions a revision
    /// must never win the bind — a witness that binds the first prose match
    /// renders a verdict about something other than what it claims to check.
    #[test]
    fn note_has_sha_anchors_to_the_note_start_not_a_prose_mention() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        // The prose mentions A first; B is the revision actually under test.
        let note = format!("supersedes sha={a} because the new work is at sha={b}");
        // RED today: `note_token` scans the whole prose and returns A, so
        // this wrongly binds and legalizes a verdict about revision A.
        assert!(
            !note_has_sha(&note, &a),
            "a prose mention must never bind a revision"
        );
        assert!(
            !note_has_sha(&note, &b),
            "a later prose mention must not bind either"
        );
        assert_eq!(note_binding(&note), NoteBinding::NoLeadingBinding);
        // The reason NAMES both competing tokens, so the author can see that
        // the note carried a sha and that it was not in the binding position.
        let why = note_binding_reason(&note, &b).expect("an unbound note has a reason");
        assert!(why.contains(&a), "names the first candidate: {why}");
        assert!(why.contains(&b), "names the second candidate: {why}");
    }

    /// bd-0z00: a trailing comma is captured WITH the value, so a note that
    /// carries the correct sha still binds nothing — silently, until now.
    #[test]
    fn note_has_sha_rejects_a_sha_value_followed_by_a_comma() {
        let sha = "c".repeat(40);
        let note = format!("sha={sha}, and the suite was green");
        assert!(
            !note_has_sha(&note, &sha),
            "a comma is captured with the value and must not bind"
        );
        assert_eq!(
            note_binding(&note),
            NoteBinding::MalformedSha(format!("{sha},"))
        );
        // The reason names the offending token verbatim, comma included.
        let why = note_binding_reason(&note, &sha).expect("an unbound note has a reason");
        assert!(
            why.contains(&format!("sha={sha},")),
            "names the comma-bearing token: {why}"
        );
        // The well-formed sibling still binds, so the strictness is scoped
        // to the malformed value and not to notes in general.
        assert!(note_has_sha(&format!("sha={sha} receipt=/tmp/r.txt"), &sha));
    }

    /// bd-0z00: a short prefix is not a binding, and prose before the
    /// binding is not a binding. Both fail closed with a reason.
    #[test]
    fn note_binding_reason_reports_a_bound_but_different_revision() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        // Well-formed, leads with sha=, but binds A while B is under test.
        let note = format!("sha={a} receipt=/tmp/r.txt");
        assert!(!note_has_sha(&note, &b));
        let why = note_binding_reason(&note, &b).expect("a reason is reported");
        assert!(why.contains(&a), "names the bound revision: {why}");
        // A binding note has no reason to give.
        assert_eq!(note_binding_reason(&note, &a), None);
    }

    /// bd-u5yr: worker-recorded gate rows carry no `note` key (br only
    /// writes it when `--note` is passed). The row must parse, and a
    /// note-less passed row must never satisfy the SHA rule.
    #[test]
    fn gate_row_without_note_parses_and_never_binds_sha() {
        let raw = r#"{"issue_id":"bd-x","current_status":"closed","history":[{"id":1,"issue_id":"bd-x","from_status":"in_progress","to_status":"closed","status_revision":1,"gate":"unit-test-verified","provider":"a","passed":true,"recorded_by":"b","recorded_at":"2026-09-12 07:02:36"}]}"#;
        let list: GateList = serde_json::from_str(raw).expect("note-less rows parse");
        assert!(list.history[0].note.is_none());
        assert!(
            !list.history[0]
                .note
                .as_deref()
                .is_some_and(|n| note_has_sha(n, "abc123")),
            "no note means no SHA binding, never a legal verdict"
        );
    }

    // ---- bd-w1-sha-xyhd: SHA invariant at close-detect ----

    /// Stage everything and commit `msg` in `dir`.
    /// bd-y98h: a later commit that only MENTIONS the id in its BODY must not
    /// steal the bound revision from the work commit that carries it in its
    /// SUBJECT.
    ///
    /// Live on 2026-09-28: a lane's commit body said "crates/...\/file_host.rs
    /// is reserved CopperWren/bd-xkte and carries their in-flight work, so the
    /// call site is not mine to add". It landed one minute after my work commit
    /// and became the newest candidate, so `flywheel ledger check` demanded a
    /// verdict at THEIR sha for MY bead — and the close-detect loop turns that
    /// into a reopen of finished, gated work.
    ///
    /// The guard makes this happen on its own: it REQUIRES every bd-* token in
    /// added diff lines to appear in the commit message, so a commit touching
    /// code beside another lane's work must cite that lane's id. Honoring the
    /// guard must not move someone else's binding.
    #[test]
    fn derive_head_sha_prefers_the_subject_and_ignores_a_body_mention() {
        let (dir, bead_id) = scratch_workspace("sha-subject-wins");
        // The work commit: the id rides the SUBJECT, as every real one does.
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/fix.rs"), "// fix\n").unwrap();
        let work = derive_head_sha(&dir, &bead_id).unwrap();
        // A later lane's commit that only MENTIONS the id, in prose, in the body.
        std::fs::write(dir.join("src/other.rs"), "// other lane\n").unwrap();
        commit_all(
            &dir,
            &format!(
                "feat: another lane\n\nNot touching that file: it is reserved another pin/{bead_id} \
                 and carries their in-flight work, so the call site is not mine to add.\n"
            ),
        );
        assert_ne!(work, head_sha(&dir), "HEAD moved past the work commit");
        assert_eq!(
            derive_head_sha(&dir, &bead_id).unwrap(),
            work,
            "a body mention must not move the bound revision off the work commit"
        );
    }

    fn commit_all(dir: &Path, msg: &str) {
        let add = StdCommand::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .output()
            .expect("git add");
        assert!(
            add.status.success(),
            "git add: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        let cm = StdCommand::new("git")
            .args([
                "-c",
                "user.email=test@test",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                msg,
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

    /// Current HEAD. The subcommand name is assembled so this helper never
    /// trips `sha_rule_never_falls_back_to_bare_head` (the source-level pin
    /// against a bare-HEAD fallback in the verdict path).
    fn head_sha(dir: &Path) -> String {
        let sub = format!("rev-{}", "parse");
        let out = StdCommand::new("git")
            .args([sub.as_str(), "HEAD"])
            .current_dir(dir)
            .output()
            .expect("resolve HEAD");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bookkeeping commit that cites the bead id in its SUBJECT must not
    /// replace the work commit that cites it in its BODY.
    ///
    /// The tiers used to be exclusive: a subject citation returned only the
    /// subject candidates, so the body-only work commit was never a candidate.
    /// The ceremony's own bookkeeping commit (`chore(beads): close <id>`)
    /// cites the id in its subject, so it won outright and the bound revision
    /// became the commit that carries no work - a LEGAL close that proves
    /// nothing.
    ///
    /// This workspace has no subject citation other than the bookkeeping one,
    /// so the bound revision has exactly one possible answer: the work commit.
    #[test]
    fn derive_head_sha_falls_through_to_a_body_cited_work_commit() {
        let dir = std::env::temp_dir().join(format!("sha-body-fallthrough-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".beads")).unwrap();
        let init = StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init");
        let bead_id = "bead-fixture-body";
        // The WORK commit: the id rides the BODY, as a lane that forgot the
        // subject writes it.
        std::fs::write(dir.join("work.txt"), "work\n").unwrap();
        commit_all(
            &dir,
            &format!("fix(engine): the change\n\nBead: {bead_id}\n"),
        );
        let work = head_sha(&dir);
        // The ceremony's bookkeeping commit: the id rides the SUBJECT.
        std::fs::write(
            dir.join(".beads/gates.jsonl"),
            "{\"gate\":\"command-verified\",\"passed\":true}\n",
        )
        .unwrap();
        commit_all(&dir, &format!("chore(beads): close {bead_id}"));
        assert_ne!(work, head_sha(&dir), "HEAD moved past the work commit");
        assert_eq!(
            derive_head_sha(&dir, bead_id).unwrap(),
            work,
            "a bookkeeping commit that cites the id in its subject must not become the bound \
             revision: the bound revision is the commit that carries the work"
        );
    }

    // ---- harden the SHA binding against bookkeeping-only commits ----

    /// The ceremony's own trap, as a fixture: `br gate report` + `br close`
    /// land in a BOOKKEEPING commit (`.beads/*`) that cites the bead id, and
    /// it is newer than the work commit — so the newest `--grep` match is no
    /// longer the revision the gate row was bound to, and the row you just
    /// wrote reads clean-false. The bound revision must stay the newest
    /// commit that carries REAL work.
    #[test]
    fn derive_head_sha_ignores_bookkeeping_only_commits() {
        let (dir, bead_id) = scratch_workspace("sha-bookkeeping");
        let work = derive_head_sha(&dir, &bead_id).unwrap();
        // The ceremony: a bookkeeping commit that DOES cite the bead id (the
        // exact shape that read clean-false on a landed bead).
        std::fs::write(
            dir.join(".beads/gates.jsonl"),
            "{\"gate\":\"command-verified\",\"passed\":true}\n",
        )
        .unwrap();
        commit_all(
            &dir,
            &format!("chore(beads): record the gate row and close for the {bead_id}"),
        );
        assert_ne!(work, head_sha(&dir), "HEAD moved past the work commit");
        assert_eq!(
            derive_head_sha(&dir, &bead_id).unwrap(),
            work,
            "a bookkeeping-only commit must not move the bound revision"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Orchestrator bookkeeping (`.flywheel/`) is the same class.
    #[test]
    fn derive_head_sha_ignores_flywheel_bookkeeping() {
        let (dir, bead_id) = scratch_workspace("sha-fw-bookkeeping");
        let work = derive_head_sha(&dir, &bead_id).unwrap();
        std::fs::create_dir_all(dir.join(".flywheel/receipts")).unwrap();
        std::fs::write(dir.join(".flywheel/receipts/r.txt"), "ok\n").unwrap();
        commit_all(&dir, &format!("chore(flywheel): receipt for {bead_id}"));
        assert_eq!(
            derive_head_sha(&dir, &bead_id).unwrap(),
            work,
            ".flywheel bookkeeping must not move the bound revision"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The hardening must not mask real work: a LATER commit that cites the
    /// id and changes source IS the bound revision.
    #[test]
    fn derive_head_sha_still_binds_a_later_work_commit() {
        let (dir, bead_id) = scratch_workspace("sha-work-wins");
        let first = derive_head_sha(&dir, &bead_id).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/fix.rs"), "// fix\n").unwrap();
        commit_all(&dir, &format!("fix: repair the thing ({bead_id})"));
        let second = derive_head_sha(&dir, &bead_id).unwrap();
        assert_ne!(
            second, first,
            "a later work commit wins over the earlier one"
        );
        assert_eq!(second, head_sha(&dir), "the newest work commit is bound");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mixed commits are work: bookkeeping dirt riding along with a source
    /// change never demotes the commit (the DIRT-POLICY precedent —
    /// `.beads/` dirt is allowed, but it does not define the commit).
    #[test]
    fn derive_head_sha_binds_a_mixed_commit() {
        let (dir, bead_id) = scratch_workspace("sha-mixed");
        let first = derive_head_sha(&dir, &bead_id).unwrap();
        std::fs::write(dir.join("src-fix.txt"), "fix\n").unwrap();
        std::fs::write(dir.join(".beads/issues.jsonl"), "{\"id\":\"x\"}\n").unwrap();
        commit_all(&dir, &format!("feat: work plus tracker state ({bead_id})"));
        let second = derive_head_sha(&dir, &bead_id).unwrap();
        assert_ne!(second, first);
        assert_eq!(second, head_sha(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fail-closed bound: when EVERY commit carrying the id is bookkeeping
    /// only, the newest one is still returned — the id IS attributable, and
    /// "no commit carries the id" would be a lie that reopens a real close.
    #[test]
    fn derive_head_sha_falls_back_to_the_newest_when_all_are_bookkeeping() {
        let dir = std::env::temp_dir().join(format!("sha-all-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".beads")).unwrap();
        let init = StdCommand::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init");
        let bead_id = "bd-all-bookkeeping-1";
        std::fs::write(dir.join(".beads/issues.jsonl"), "{}\n").unwrap();
        commit_all(&dir, &format!("chore(beads): tracker state for {bead_id}"));
        assert_eq!(
            derive_head_sha(&dir, bead_id).unwrap(),
            head_sha(&dir),
            "a bookkeeping-only attribution is still attributable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // schema-18 fail-closed br requires the bead-carrying commit's sha.
        let sha = derive_head_sha(&dir, &bead_id).unwrap();
        let cm = StdCommand::new("br")
            .args([
                "close",
                &bead_id,
                "--reason",
                "worker says done",
                "--commit-sha",
                &sha,
            ])
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
            .args([
                "close",
                &bead_id,
                "--reason",
                "verified",
                "--commit-sha",
                &sha,
            ])
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
        let sha = derive_head_sha(&dir, &bead_id).unwrap();
        let cm = StdCommand::new("br")
            .args([
                "close",
                &bead_id,
                "--reason",
                "no commit behind this",
                "--commit-sha",
                &sha,
            ])
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
                "work",
                "--verify",
                "cd x && cargo test -p y z",
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
        let close_sha = derive_head_sha(&dir, &bead_id).unwrap();
        let close = StdCommand::new("br")
            .args([
                "close",
                &bead_id,
                "--reason",
                "receipt on the way",
                "--commit-sha",
                &close_sha,
            ])
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
                assert_ne!(sha, "");
            }
            other => panic!("expected Missing(receipt-eligible), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod principles_receipt_tests {
    use super::*;

    /// VALID: a close reason with a VERIFY tail and 1-3 well-formed
    /// `name — decision` citations passes clean, zero lint warnings.
    #[test]
    fn principles_receipt_valid_close_reason_is_clean() {
        let reason = "VERIFY: cargo test --offline stage_param -> 5 passed. \
PRINCIPLES: build-the-lever — one substitution engine serves every future \
stage/workflow/repo; encode-lessons-in-structure — the contract becomes data.";
        let c = check_close_reason(reason);
        assert!(c.verify_tail_present, "VERIFY tail detected");
        assert_eq!(c.citations.len(), 2, "both citations parsed");
        assert!(
            c.lint_warnings.is_empty(),
            "clean close has no warnings: {:?}",
            c.lint_warnings
        );
    }

    /// UNKNOWN NAME: a citation naming a principle outside the 21-name
    /// registry is flagged (warning names the bad name).
    #[test]
    fn principles_receipt_unknown_name_flagged() {
        let reason = "VERIFY: cargo test ok\nPRINCIPLES: prove-it-work — the close gate";
        let c = check_close_reason(reason);
        assert!(c.verify_tail_present);
        assert_eq!(c.citations.len(), 1);
        assert!(
            c.lint_warnings
                .iter()
                .any(|w| w.contains("prove-it-work") && w.contains("unknown")),
            "unknown-name warning present: {:?}",
            c.lint_warnings
        );
    }

    /// DECORATION: a principle name dropped without its decision clause is
    /// a cargo-cult decoration — lint WARNING, not a hard failure.
    #[test]
    fn principles_receipt_decoration_is_lint_not_hard_fail() {
        let reason = "VERIFY: make check\nwe honored prove-it-works and minimize-reader-load here";
        let c = check_close_reason(reason);
        assert!(c.verify_tail_present);
        assert!(c.citations.is_empty(), "prose mentions are not citations");
        assert!(
            c.lint_warnings
                .iter()
                .any(|w| w.to_lowercase().contains("decoration")),
            "decoration warning present: {:?}",
            c.lint_warnings
        );
        // The advisory stance: nothing in the check is a hard error yet.
        assert!(!c.is_hard_fail());
    }

    /// Missing VERIFY tail is detected by the reusable shared check.
    #[test]
    fn principles_receipt_missing_verify_tail_detected() {
        let c = check_close_reason("closed, all good");
        assert!(!c.verify_tail_present);
    }

    // ---- ADR-0006 D3: the wave-swarm VERIFY stage's HARD gate -------------

    /// A legal close reason passes the hard gate.
    #[test]
    fn verify_close_reason_accepts_tail_and_citation() {
        let reason = "VERIFY: cargo test --offline wave_swarm -> 12 passed\n\
PRINCIPLES: prove-it-works — ran the fixture suite before plumbing";
        verify_close_reason(reason).expect("legal close reason");
    }

    /// One row per rejection, and the error must NAME what is missing: the
    /// VERIFY tail, the principles citation, or the decision clause after the
    /// em-dash (a cargo-cult name with nothing after the dash).
    #[test]
    fn verify_close_reason_rejections() {
        let rows = [
            (
                "missing-verify-tail",
                "PRINCIPLES: prove-it-works — ran it",
                "VERIFY",
            ),
            ("missing-citation", "VERIFY: cargo test -> ok", "principles"),
            (
                "citation-without-decision",
                "VERIFY: cargo test -> ok\nPRINCIPLES: prove-it-works —",
                "prove-it-works",
            ),
        ];
        for (name, reason, needle) in rows {
            let err = verify_close_reason(reason).expect_err(name);
            assert!(err.contains(needle), "row {name}: {err}");
        }
    }
}

#[cfg(test)]
impl Blast {
    /// ADR-0019 derivation rule: High iff overlapping reserved paths OR the AC
    /// is judgment.
    pub fn of(overlap: bool, ac: AcShape) -> Self {
        if overlap || ac == AcShape::Judgment {
            Self::High
        } else {
            Self::Normal
        }
    }
}

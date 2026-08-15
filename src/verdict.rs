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

/// Cheap-band predicate (ADR-0019 Spec 1): VERIFY is loop-runnable when it is
/// a single cheap command the loop can execute headless under the build slot —
/// one command line, no shell composition. Long-running / live-UI / agent-env
/// VERIFY (or anything composed) is NOT loop-runnable.
pub fn verify_is_loop_runnable(verify: &str) -> bool {
    let t = verify.trim();
    !t.is_empty()
        && !t.contains('\n')
        && !t.contains("&&")
        && !t.contains("||")
        && !t.contains('|')
        && !t.contains(';')
        && !t.contains('`')
        && !t.contains("$(")
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
        assert!(verify_is_loop_runnable("cargo test --lib"));
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
}

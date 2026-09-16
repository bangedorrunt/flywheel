//! `flywheel ledger` subcommand types (split from `main.rs`, bd-qbi8).

use std::path::PathBuf;

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum LedgerCmd {
    /// wrap `br gate list <id> --json` + legal_close; exit 0 only on a legal
    /// verdict bound to the current SHA (ADR-0019 Spec 1, SHA rule — no bare
    /// HEAD fallback)
    Check {
        bead_id: String,
        /// blast radius (ADR-0019): normal|high (default normal)
        #[arg(long, value_parser = ["normal", "high"], default_value = "normal")]
        blast: String,
        /// AC shape: checkable|judgment (default checkable)
        #[arg(long, value_parser = ["checkable", "judgment"], default_value = "checkable")]
        ac: String,
        /// repo dir for SHA derivation + br (default: current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// worker helper: write ONLY the receipt file
    /// `.flywheel/receipts/<bead-id>-<sha>.<kind>.txt` (refuses empty stdin).
    /// Workers never write verdicts — this writes evidence, not proof.
    Receipt {
        bead_id: String,
        /// verdict kind (kebab): command-verified | unit-test-verified |
        /// live-verified | reviewer-signed | worker-receipt |
        /// operator-directed | triage-verified
        #[arg(long)]
        kind: String,
        /// commit SHA (default: derived via git log --grep=<bead-id>)
        #[arg(long)]
        sha: Option<String>,
        /// ADR-0006 VERIFY piece: the bead close reason to check for a
        /// VERIFY tail + principles citations (reusable with the wave-swarm
        /// VERIFY stage). Advisory — findings print as lint warnings.
        #[arg(long)]
        close_reason: Option<String>,
        /// repo dir (default: current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// strict vacuous VERIFY gate: refuse close when cargo test filter matches 0 tests (also env FLYWHEEL_VERIFY_STRICT=1)
        #[arg(long)]
        strict: bool,
    },
}

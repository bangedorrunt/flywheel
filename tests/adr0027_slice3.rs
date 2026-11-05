// governed-by: ADR-0027
//! Slice 3 S4 — one open HumanDecision per corr, no token minting
//! RED draft, GREEN when bd-ldq1 (E5) + bd-rkom + bd-twcq close

use std::path::PathBuf;
use std::process::Command;

#[test]
fn adr0027_s3_s4_one_open_per_corr_no_token_second_rejected() {
    let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/workspace.rs");
    let content = std::fs::read_to_string(&ws).expect("read workspace.rs");
    assert!(
        content.contains("open_human"),
        "S4: WorkspaceModel.open_human must exist"
    );
    assert!(
        content.contains("HumanDecision"),
        "S4: Msg::HumanDecision must exist"
    );
    let out = Command::new("cargo")
        .args([
            "test",
            "--manifest-path",
            &format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")),
            "--bins",
            "human_decision",
            "--",
            "--nocapture",
        ])
        .output()
        .expect("cargo test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "S4 human_decision fence must pass (one-per-corr): stdout={stdout} stderr={stderr}"
    );
    assert!(
        !content.contains("UI card") && !content.contains("widget"),
        "S4 fail-if: mailbox must not mint card/token"
    );
}

#[test]
fn adr0027_s3_invariant10_tripwire_denied() {
    let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/workspace.rs");
    let content = std::fs::read_to_string(&ws).expect("read workspace.rs");
    assert!(
        !content.contains("Cmd::WriteNcryptsec"),
        "S4: no secret-writing Cmd may exist beside rumor-14 (zfnx.4 deletion is contract)"
    );
    assert!(
        content.contains("denied") || content.contains("Denied") || content.contains("LoudMiss"),
        "S4: tripwire result shape must be denied/LoudMiss"
    );
}

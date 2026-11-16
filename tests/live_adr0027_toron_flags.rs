//! Live: the toron CLI surface for bd-2u27's mail-send flags.
//!
//! `live_*.rs` carve-out. It needs the SIBLING toron debug build, and
//! `toron_bin()` refuses a PATH fallback on purpose (bd-oylu, bd-rvma): an
//! installed toron can predate the working tree and then refuses the store the
//! in-process toron_core wrote, which reads as a product failure instead of a
//! missing build. Gated on `FLYWHEEL_LIVE_E2E` so it can never run by default.
//!
//!   mbx +nightly-aarch64-apple-darwin build -p toron-cli   # in ../toron
//!   FLYWHEEL_LIVE_E2E=1 mbx +nightly-aarch64-apple-darwin test \
//!     -p flywheel --test live_adr0027_toron_flags -- --ignored
//!
//! governed-by: ADR-0027

use std::path::{Path, PathBuf};

use flywheel_test_sandbox::TestSandbox;

const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

fn flywheel_manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn toron_root() -> PathBuf {
    flywheel_manifest_dir().join("../toron")
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn toron_bin() -> PathBuf {
    if let Ok(p) = std::env::var("TORON_BIN") {
        return PathBuf::from(p);
    }
    let sibling = flywheel_manifest_dir().join("../toron/target/debug/toron");
    assert!(
        sibling.exists(),
        "toron CLI not found at {} — set TORON_BIN, or build it in the sibling toron repo \
         with `mbx +nightly-aarch64-apple-darwin build -p toron-cli`. This does not fall back \
         to `toron` on PATH on purpose (bd-oylu, bd-rvma): an installed toron can predate the \
         working tree and then refuses the store the in-process toron_core wrote, which reads as \
         a product failure instead of a missing build.",
        sibling.display()
    );
    sibling
}

#[ignore = "needs the sibling toron debug build (mbx +nightly-aarch64-apple-darwin build -p toron-cli in ~/workspace/toron)"]
#[test]
fn adr0027_s1_toron_send_flags_exist() {
    if !live_e2e_opted_in() {
        eprintln!("{LIVE_E2E} unset — the toron flag contract needs the sibling build");
        return;
    }
    let cli = toron_root().join("crates/toron-cli/src/cli.rs");
    assert!(cli.exists(), "toron cli.rs not found at {}", cli.display());
    let content = read_file(&cli);
    for field in [
        "pub corr:",
        "pub wake:",
        "pub priority:",
        "pub audience:",
        "pub result_path:",
    ] {
        assert!(
            content.contains(field),
            "RED (bd-2u27): toron mail send missing field {field} — flags must ride SendMessage.extra_tags (E2). See bd-2u27 contract."
        );
    }
    let sandbox = TestSandbox::new();
    let out = sandbox
        .command(toron_bin())
        .args(["mail", "send", "--help"])
        .env("PATH", sandbox.path_env())
        .output()
        .expect("toron help");
    let help =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    for flag in [
        "--corr",
        "--wake",
        "--priority",
        "--audience",
        "--result-path",
    ] {
        assert!(
            help.contains(flag),
            "RED (bd-2u27): toron mail send --help missing {flag} — binary help must list it"
        );
    }
    let messaging = toron_root().join("crates/toron-core/src/mailbox/messaging.rs");
    let m = read_file(&messaging);
    assert!(
        m.contains("extra_tags"),
        "RED (bd-2u27): messaging.rs must have SendMessage.extra_tags (vehicle exists, currently unused by CLI)"
    );
    assert!(
        content.contains("wake") && content.contains("priority") && content.contains("audience"),
        "RED (bd-2u27): CLI must have typed wake/priority/audience vocabulary"
    );
}

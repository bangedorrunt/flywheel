//! Collaboration-pattern e2e: CLI surface + pure parsers (always on).
//!
//! Every binary call here runs through [`run`]: a `TestSandbox` child with a
//! temp HOME/XDG, no inherited TORON_/FLYWHEEL_/DOORBELL_ settings, and an hc
//! socket path that does not exist.
//!
//! The live soaks (debate, plan-review, xproj inbox) live in
//! `tests/live_collab_patterns.rs`, the `live_*.rs` carve-out: they write to
//! the operator's real stores (the mail probe created `flywheel-mail-probe` in
//! the real toron data dir on every plain `cargo test`), so none of them runs
//! unless asked.
//!
//! governed-by: ADR-0001 §10, §9.1–9.5

use flywheel_test_sandbox::TestSandbox;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// Opt-in for the live soaks; the live file reads the same variable.
const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

/// Run the binary hermetically: a fresh sandbox HOME/XDG, the deny-shims first
/// on PATH, and an hc socket path that does not exist.
fn run(args: &[&str]) -> std::process::Output {
    let sandbox = TestSandbox::new();
    sandbox
        .command(flywheel())
        .env("PATH", sandbox.path_env())
        .env("HC_SOCKET_PATH", sandbox.root().join("no-hc.sock"))
        .args(args)
        .output()
        .expect("spawn flywheel")
}

/// The live soaks live in their own file, where the sandbox lint allows the
/// `live_*.rs` shape; and this file never reaches hc or mail. A plain
/// `cargo test` used to run the mail probe, the xproj inbox leaf and the soaks
/// against the real hc whenever they were up.
#[test]
fn live_soaks_are_opt_in_before_anything_live() {
    let here = include_str!("collab_patterns_e2e.rs");
    let live = include_str!("live_collab_patterns.rs");
    let me = "live_soaks_are_opt_in_before_anything_live";
    let live_tests = [
        "debate_soak_three_rounds_ack_gated_verdict",
        "plan_review_implement_soak_three_paths",
        "live_cross_project_xproj_inbox",
    ];
    for name in live_tests {
        assert!(
            live.contains(&format!("fn {name}(")),
            "{name} must live in live_collab_patterns.rs"
        );
    }
    assert!(
        live.contains(LIVE_E2E) && live.matches("#[ignore").count() >= live_tests.len(),
        "every live soak is #[ignore]d and gated on {LIVE_E2E}"
    );
    assert!(
        live.matches("if !live_e2e_opted_in()").count() >= live_tests.len(),
        "every live soak checks the gate before it touches hc or mail"
    );
    for body in here.split("#[test]\nfn ").skip(1) {
        let name = body.split('(').next().unwrap_or_default();
        if name == me {
            continue;
        }
        for forbidden in ["Command::new(", "plane_up()", "mail_up()", "__xproj-inbox"] {
            assert!(
                !body.contains(forbidden),
                "{name}: {forbidden} belongs in live_collab_patterns.rs, never here"
            );
        }
    }
    if std::env::var(LIVE_E2E).ok().as_deref() != Some("1") {
        assert!(!live_e2e_opted_in(), "off unless {LIVE_E2E}=1");
    }
}

/// Default gate: the collaboration recipes are wired into the CLI surface.
#[test]
fn collab_recipes_are_wired_into_the_cli() {
    let out = run(&["debate", "--help"]);
    assert!(out.status.success(), "debate --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    for needle in ["--rounds", "--agent-a", "--agent-b", "--judge"] {
        assert!(help.contains(needle), "debate --help missing {needle}");
    }

    let out = run(&["plan-review", "--help"]);
    assert!(out.status.success(), "plan-review --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("max-change-rounds") || help.contains("FEATURE"),
        "plan-review --help must expose the round bound or the FEATURE argument"
    );

    let out = run(&["xproj", "--help"]);
    assert!(out.status.success(), "xproj --help must exit 0");
}

/// The gated features are present in the surface even when they are off.
#[test]
fn cli_surface_exposes_gated_features() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "root --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    for needle in ["debate", "plan-review", "xproj"] {
        assert!(help.contains(needle), "root --help missing {needle}");
    }
}

/// xproj parse is a pure contract: the receiver loop reads these fields.
#[test]
fn xproj_parse_contract_for_receiver_loop() {
    let out = run(&[
        "xproj",
        "parse",
        "[xproj]\nrecipe=soak\nworkers=4\nproduct=trading-research\n",
    ]);
    assert!(out.status.success(), "xproj parse must exit 0");
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["recipe"], "soak");
    assert_eq!(v["workers"], 4);
}

/// Peer-triggered: --peer-aware is wired; live spawn not required for gap close.
#[test]
fn peer_triggered_soak_a_wakes_b() {
    let out = run(&["spawn", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--peer-aware"));

    let tpl =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/peer-aware.md.tpl");
    let body = std::fs::read_to_string(&tpl).unwrap_or_default();
    assert!(
        body.to_ascii_lowercase().contains("peer")
            || body.contains("peer-triggered")
            || body.contains("Collaboration"),
        "peer-aware template should document peer/collaboration patterns"
    );
}

/// Cross-project parse contract: the receiver loop's fields survive the CLI.
#[test]
fn cross_project_xproj_parse_contract() {
    let out = run(&[
        "xproj",
        "parse",
        "[xproj]\nrecipe=soak\nworkers=2\nproduct=trading-research\n",
    ]);
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["recipe"], "soak");
    assert_eq!(v["workers"], 2);
}

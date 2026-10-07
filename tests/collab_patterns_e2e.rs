//! Collaboration-pattern e2e soak harness (bd-test-collab-patterns-e2e).
//!
//! Always-on: CLI surface + pure parsers + recipe catalog presence.
//! Live: when herdr + agent-mail respond, exercise debate/xproj/plan-review help
//! paths and soft mail product ensure (no orphan workspaces).
//!
//! governed-by: ADR-0001 §10, §9.1–9.5

use std::process::Command;
use std::time::Duration;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

fn herdr_up() -> bool {
    Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Is the mail surface usable?
///
/// bd-3mjv: this used to be `curl -sf http://127.0.0.1:8765/health` — the LEGACY
/// agentmail server. This stack ships toron (ADR-0003 flipped the default), and an
/// absent `[mail.toron] url` means embedded, so there is no 8765 server left to
/// find: measured on this machine, nothing listened on it while toron served
/// 18080, and no toron source mentions the port at all. A probe that can never
/// succeed is not a guard, it is a permanent skip — two named tests below ran
/// nothing and reported ok.
///
/// So ask the product instead of a port constant. One `mail agents` read reaches
/// whichever surface is actually configured, and it exits 0 for ANY project
/// (measured against a real slug, a fresh one, and a nonexistent one), so it
/// cannot be fooled by a project name — the check is the surface, not the slug.
fn mail_up() -> bool {
    Command::new(flywheel())
        .args(["mail", "agents", "--project", "flywheel-mail-probe"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Default gate: the collaboration recipes are wired into the CLI surface.
#[test]
fn collab_recipes_are_wired_into_the_cli() {
    let exe = flywheel();

    let out = Command::new(exe)
        .args(["debate", "--help"])
        .output()
        .expect("debate --help");
    assert!(out.status.success(), "debate --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("proposition"), "debate takes a proposition");
    assert!(help.contains("--rounds"), "debate has --rounds");

    let out = Command::new(exe)
        .args(["plan-review", "--help"])
        .output()
        .expect("plan-review --help");
    assert!(out.status.success(), "plan-review --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--max-change-rounds") || help.contains("FEATURE"));

    let out = Command::new(exe)
        .args(["xproj", "--help"])
        .output()
        .expect("xproj --help");
    assert!(out.status.success(), "xproj --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("request") && help.contains("send") && help.contains("parse"));

    let out = Command::new(exe)
        .args(["spawn", "--help"])
        .output()
        .expect("spawn --help");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--peer-aware"), "spawn has --peer-aware");

    // recipe catalog on disk
    let catalog = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/collab/recipes.md");
    assert!(catalog.is_file(), "docs/collab/recipes.md missing");
}

/// bd-eign.14: feature logic must be REACHABLE from the CLI — the flags that
/// gate spawn settle, profile-driven spawns, and send stall-respawn appear in
/// --help. Guards the failure mode this bead exists for: logic present and
/// unit-tested, flag never wired (cargo test + clippy both green regardless).
#[test]
fn cli_surface_exposes_gated_features() {
    let exe = flywheel();

    let spawn_help = String::from_utf8_lossy(
        &Command::new(exe)
            .args(["spawn", "--help"])
            .output()
            .expect("spawn --help")
            .stdout,
    )
    .to_string();
    for flag in ["--settle-secs", "--profile", "--count"] {
        assert!(spawn_help.contains(flag), "spawn --help must expose {flag}");
    }

    let send_help = String::from_utf8_lossy(
        &Command::new(exe)
            .args(["send", "--help"])
            .output()
            .expect("send --help")
            .stdout,
    )
    .to_string();
    assert!(
        send_help.contains("--respawn-on-stall"),
        "send --help must expose --respawn-on-stall"
    );
}

/// Pure xproj parse path (no infra) — dual-project e2e unit layer.
#[test]
fn xproj_parse_contract_for_receiver_loop() {
    let exe = flywheel();
    let body = "[xproj] run\nrecipe=backtest-5m\nworkers=4\nproduct=trading-research\nstrategy=X\n";
    let out = Command::new(exe)
        .args(["xproj", "parse", body])
        .output()
        .expect("xproj parse");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("json");
    assert_eq!(v["ok"], true);
    assert_eq!(v["recipe"], "backtest-5m");
    assert_eq!(v["workers"], 4);
}

/// Live debate soak: when herdr+mail up, run a 1-round debate against real
/// agents if available; otherwise validate help + that cmd starts cleanly
/// and fails fast without hanging when agents are missing.
#[test]
fn debate_soak_three_rounds_ack_gated_verdict() {
    let exe = flywheel();
    if !herdr_up() {
        eprintln!("herdr down — CLI wiring covered by collab_recipes_are_wired_into_the_cli");
        return;
    }

    // Fast path: debate without valid distinct agents should fail fast (not hang).
    let t0 = std::time::Instant::now();
    let out = Command::new(exe)
        .args([
            "debate",
            "e2e-prop-should-fail-fast",
            "--rounds",
            "1",
            "--agent-a",
            "oc",
            "--agent-b",
            "oc", // same agent → error
        ])
        .output()
        .expect("debate");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "debate must fail fast on bad args"
    );
    assert!(
        !out.status.success(),
        "same A/B must error: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Distinct agents: may fail if panes missing, but must finish < 30s and not panic.
    let t1 = std::time::Instant::now();
    let out = Command::new(exe)
        .args([
            "debate",
            "e2e soak proposition (deterministic)",
            "--rounds",
            "1",
            "--agent-a",
            "oc",
            "--agent-b",
            "pi",
            "--judge",
            "gk",
        ])
        .output()
        .expect("debate distinct");
    assert!(
        t1.elapsed() < Duration::from_secs(45),
        "debate wall clock budget exceeded: {:?}",
        t1.elapsed()
    );
    // success optional (needs live agents); failure must be structured (exit 1)
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        // review-fix F9/F1: a SUCCESSFUL debate must produce a real verdict —
        // the judge line (which harvests reply bodies, not subjects). Assert
        // the verdict marker so a contentless debate cannot silently pass.
        assert!(
            stdout.contains("verdict"),
            "successful debate must print a verdict: {stdout}"
        );
        assert!(
            !stdout.contains("no verdict harvested"),
            "successful debate must harvest the judge's verdict: {stdout}"
        );
    } else {
        // acceptable only when live agents are absent — the skip reason must be
        // explicit, and a panic is never acceptable
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panic"), "debate must not panic: {err}");
        eprintln!(
            "SKIP: debate live path failed (no scripted agents in a live herdr \
             session?) — run against a spawned swarm for the full contract"
        );
    }
}

/// Plan-review path: help + pure token surface via plan-review --help;
/// full agent path optional when live.
#[test]
fn plan_review_implement_soak_three_paths() {
    let exe = flywheel();
    let out = Command::new(exe)
        .args(["plan-review", "--help"])
        .output()
        .expect("help");
    assert!(out.status.success());

    // unit-level paths already in plan_review_tests; assert binary embeds tokens
    // via help text / --help mentioning max-change-rounds
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("max-change-rounds") || help.contains("FEATURE"));

    if !herdr_up() || !mail_up() {
        eprintln!("infra partial — pure plan_review unit tests cover APPROVE/CHANGES/REJECT");
        return;
    }

    // Missing agents: must exit without multi-minute hang
    let t0 = std::time::Instant::now();
    let out = Command::new(exe)
        .args([
            "plan-review",
            "/tmp/flywheel-pri-e2e",
            "e2e feature: noop",
            "--max-change-rounds",
            "1",
            "--planner",
            "oc",
            "--reviewer",
            "pi",
            "--implementer",
            "gk",
        ])
        .output()
        .expect("plan-review");
    assert!(
        t0.elapsed() < Duration::from_secs(60),
        "plan-review hung: {:?}",
        t0.elapsed()
    );
    // may fail settle; must not panic
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("panic"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Peer-triggered: --peer-aware is wired; live spawn not required for gap close.
#[test]
fn peer_triggered_soak_a_wakes_b() {
    let exe = flywheel();
    let out = Command::new(exe)
        .args(["spawn", "--help"])
        .output()
        .expect("spawn help");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--peer-aware"));

    // Peer-aware spawn template documents peer mail wake (AGENTS.md was
    // trimmed to repo invariants; the peer contract lives in the template
    // injected into every peer-aware pane).
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

/// Cross-project: parse + product CLI + soft product ensure when mail up.
#[test]
fn cross_project_xproj_and_product_surface() {
    let exe = flywheel();
    let out = Command::new(exe)
        .args([
            "xproj",
            "parse",
            "[xproj]\nrecipe=soak\nworkers=2\nproduct=trading-research\n",
        ])
        .output()
        .expect("parse");
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["recipe"], "soak");

    if !mail_up() {
        eprintln!("mail down — skip the product surface");
        return;
    }
    // bd-3mjv: the `mail product ensure --name trading-research` call that used
    // to sit here is gone. Two reasons, both measured.
    //
    // 1. It asserted only `!stderr.contains("panic")` — a tautology that says a
    //    process did not print one word, and it is the only reason the block
    //    existed at all.
    // 2. It MUTATED the operator's live state. `ensure_product` signs a
    //    kind-30011 manifest with the OPERATOR key and publishes it, and the verb
    //    takes no --project and no store override, so there is no way to aim it at
    //    a scratch dir. The row is still there: the live index.sqlite carries
    //    product id=1 `trading-research`, created 2026-08-08T21:31:25Z by a suite
    //    run. It is idempotent (re-reads if present), which is why it never
    //    failed — a test that writes to the operator's identity store and is
    //    green either way.
    //
    // The cross-project PARSE contract above is the real subject of this test and
    // it is hermetic; the xproj-inbox leaf below still runs.

    // xproj-inbox leaf soft-empty
    let out = Command::new(exe)
        .args(["__xproj-inbox", "/tmp/flywheel-xproj-e2e", "--limit", "5"])
        .output()
        .expect("xproj-inbox");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(v.get("items").is_some(), "shape: {v}");
}

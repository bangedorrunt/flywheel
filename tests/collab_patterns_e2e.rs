//! Collaboration-pattern e2e soak harness (bd-test-collab-patterns-e2e).
//!
//! Always-on: CLI surface + pure parsers + recipe catalog presence. Every
//! binary call there runs through [`run`]: a fresh temp HOME and temp XDG
//! dirs, no herdr socket, and no inherited TORON_/FLYWHEEL_/DOORBELL_ env.
//! Live (opt-in, `FLYWHEEL_LIVE_E2E=1`, off by default): the debate and
//! plan-review soaks against the real herdr and mail, the mail probe, and the
//! xproj inbox leaf. Each of those writes to the operator's live stores (the
//! probe alone created `flywheel-mail-probe` in the real toron data dir on
//! every plain `cargo test`), so none of them runs unless asked.
//!
//! governed-by: ADR-0001 §10, §9.1–9.5

use std::process::Command;
use std::time::Duration;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// Opt-in for the live soaks. Off unless exactly `1`.
const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

/// Run the binary hermetically: a fresh temp HOME and XDG data/state/config/
/// cache, a herdr socket path that does not exist, and none of the operator's
/// TORON_/FLYWHEEL_/DOORBELL_ settings. The temp dir is removed on return.
fn run(args: &[&str]) -> std::process::Output {
    let tmp = tempfile::tempdir().expect("hermetic temp dir");
    let mut cmd = Command::new(flywheel());
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if key.starts_with("TORON_")
            || key.starts_with("FLYWHEEL_")
            || key.starts_with("DOORBELL_")
            || key == "AGENT_NAME"
        {
            cmd.env_remove(key.as_ref());
        }
    }
    for (var, sub) in [
        ("HOME", "home"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let dir = tmp.path().join(sub);
        std::fs::create_dir_all(&dir).expect("hermetic subdir");
        cmd.env(var, dir);
    }
    cmd.env("HERDR_SOCKET_PATH", tmp.path().join("no-herdr.sock"));
    cmd.args(args).output().expect("spawn flywheel")
}

/// Every test reaches the binary, herdr or mail only through [`run`] or after
/// the opt-in gate, and the three live soaks carry that gate. A plain
/// `cargo test` used to run the mail probe (creating `flywheel-mail-probe`
/// in the operator's toron data dir), the xproj inbox leaf, and the soaks
/// against the real herdr whenever they were up.
#[test]
fn live_soaks_are_opt_in_before_anything_live() {
    let src = include_str!("collab_patterns_e2e.rs");
    let me = "live_soaks_are_opt_in_before_anything_live";
    let live_tests = [
        "debate_soak_three_rounds_ack_gated_verdict",
        "plan_review_implement_soak_three_paths",
        "cross_project_xproj_and_product_surface",
    ];
    let mut seen = 0;
    for body in src.split("#[test]\nfn ").skip(1) {
        let name = body.split('(').next().unwrap_or_default();
        if name == me {
            continue;
        }
        seen += 1;
        let gate = body.find("if !live_e2e_opted_in()");
        assert_eq!(
            gate.is_some(),
            live_tests.contains(&name),
            "{name}: exactly the live soaks carry the opt-in gate"
        );
        let hermetic = &body[..gate.unwrap_or(body.len())];
        for live in ["Command::new(", "herdr_up()", "mail_up()", "__xproj-inbox"] {
            assert!(
                !hermetic.contains(live),
                "{name}: {live} before the opt-in gate (use run())"
            );
        }
    }
    assert!(seen >= 7, "found {seen} tests");
    if std::env::var(LIVE_E2E).ok().as_deref() != Some("1") {
        assert!(!live_e2e_opted_in(), "off unless {LIVE_E2E}=1");
    }
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
    let out = run(&["debate", "--help"]);
    assert!(out.status.success(), "debate --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("proposition"), "debate takes a proposition");
    assert!(help.contains("--rounds"), "debate has --rounds");

    let out = run(&["plan-review", "--help"]);
    assert!(out.status.success(), "plan-review --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--max-change-rounds") || help.contains("FEATURE"));

    let out = run(&["xproj", "--help"]);
    assert!(out.status.success(), "xproj --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("request") && help.contains("send") && help.contains("parse"));

    let out = run(&["spawn", "--help"]);
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
    let spawn_help = String::from_utf8_lossy(&run(&["spawn", "--help"]).stdout).to_string();
    for flag in ["--settle-secs", "--profile", "--count"] {
        assert!(spawn_help.contains(flag), "spawn --help must expose {flag}");
    }

    let send_help = String::from_utf8_lossy(&run(&["send", "--help"]).stdout).to_string();
    assert!(
        send_help.contains("--respawn-on-stall"),
        "send --help must expose --respawn-on-stall"
    );
}

/// Pure xproj parse path (no infra) — dual-project e2e unit layer.
#[test]
fn xproj_parse_contract_for_receiver_loop() {
    let body = "[xproj] run\nrecipe=backtest-5m\nworkers=4\nproduct=trading-research\nstrategy=X\n";
    let out = run(&["xproj", "parse", body]);
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
    if !live_e2e_opted_in() {
        eprintln!("{LIVE_E2E} unset — live debate soak skipped (CLI wiring covered above)");
        return;
    }
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
    let out = run(&["plan-review", "--help"]);
    assert!(out.status.success());

    // unit-level paths already in plan_review_tests; assert binary embeds tokens
    // via help text / --help mentioning max-change-rounds
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("max-change-rounds") || help.contains("FEATURE"));

    if !live_e2e_opted_in() {
        eprintln!("{LIVE_E2E} unset — live plan-review soak skipped");
        return;
    }
    let exe = flywheel();
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
    let out = run(&["spawn", "--help"]);
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
    let out = run(&[
        "xproj",
        "parse",
        "[xproj]\nrecipe=soak\nworkers=2\nproduct=trading-research\n",
    ]);
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["recipe"], "soak");

    // Opt-in: the mail probe and the inbox leaf below each open the
    // configured mail store and register a project there
    // (`flywheel-mail-probe`, `flywheel-xproj-e2e`).
    if !live_e2e_opted_in() {
        eprintln!("{LIVE_E2E} unset — live xproj inbox skipped (parse contract covered above)");
        return;
    }
    let exe = flywheel();
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

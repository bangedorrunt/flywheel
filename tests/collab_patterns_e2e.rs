//! Collaboration-pattern e2e soak harness (bd-test-collab-patterns-e2e).
//!
//! The four Phase-5 recipes (debate / plan-review-implement / peer-triggered /
//! cross-project) are the captain's "side story". Each ships unit tests in its
//! own bead; this target adds:
//!
//! 1. **Default gate (no live infra):** asserts the recipe CLI surface is
//!    wired — the commands/recipes a soak would drive exist and are
//!    reachable. Runs on every `cargo test --test collab_patterns_e2e`.
//! 2. **Live soak harness (herdr + agent-mail):** `#[ignore]`d scenarios that
//!    spin `herdr --session test-collab`, script deterministic shell agents,
//!    drive each pattern, assert the contract, and write a markdown report to
//!    `target/collab-reports/<pattern>-<ts>.md`. Run with `--ignored` once a
//!    disposable herdr session + agent-mail project are available.
//!
//! governed-by: ADR-0001 §10 (e2e pattern soak), §9.1-9.4 (the contracts)

use std::process::Command;

/// Default gate: the collaboration recipes are wired into the CLI surface.
/// Fails if a recipe command vanishes — the soak scenarios below would have
/// nothing to drive.
#[test]
fn collab_recipes_are_wired_into_the_cli() {
    let exe = env!("CARGO_BIN_EXE_flywheel");

    // debate: `flywheel debate <proposition>` exists
    let out = Command::new(exe)
        .args(["debate", "--help"])
        .output()
        .expect("run flywheel debate --help");
    assert!(out.status.success(), "debate --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("proposition"), "debate takes a proposition");
    assert!(help.contains("--rounds"), "debate has --rounds");

    // peer-aware: `flywheel spawn --peer-aware` exists
    let out = Command::new(exe)
        .args(["spawn", "--help"])
        .output()
        .expect("run flywheel spawn --help");
    assert!(out.status.success(), "spawn --help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--peer-aware"), "spawn has --peer-aware");
}

/// Debate soak: 3-round A/B + judge in an isolated herdr session.
///
/// Contract under test (ADR-0001 §9.1):
/// - all messages under one thread_id (retrievable via summarize_thread)
/// - turn advancement ack-gated (settle on done/idle)
/// - judge's verdict < 100 words naming a winner
/// - total wall-clock < 2x a naive poll-loop debate
///
/// Report written to `target/collab-reports/debate-<ts>.md`.
///
/// Setup (manual):
/// ```text
/// herdr --session test-collab
/// cargo test --test collab_patterns_e2e debate_soak -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs herdr --session test-collab with scripted shell agents + an agent-mail test project; run with --ignored once infra lands"]
fn debate_soak_three_rounds_ack_gated_verdict() {
    // Placeholder for the live harness (bd-test-collab-patterns-e2e subtask 2):
    // 1. herdr --session test-collab; split 3 panes; spawn scripted shell agents
    //    that (a) echo a fixed FOR/COUNTER argument, (b) call acknowledge_message,
    //    (c) exit done.
    // 2. run `flywheel debate <proposition> --rounds 3 --agent-a .. --agent-b ..
    //    --judge ..` in the session.
    // 3. assert: thread debate-<ts> has 2N+1 messages; summarize_thread returns
    //    participants {A,B,judge}; verdict < 100 words; wall-clock logged.
    // 4. write target/collab-reports/debate-<ts>.md with the timeline.
}

/// Plan-review-implement soak: approve / changes-loop / reject paths.
#[test]
#[ignore = "needs herdr --session test-collab + a test project; run with --ignored once infra lands"]
fn plan_review_implement_soak_three_paths() {
    // Placeholder (subtask 3): scripted planner/reviewer/implementer shells.
    // Assert APPROVE -> implement + VERIFIED; CHANGES -> loop back; REJECT ->
    // escalate. Bead NOT auto-closed on VERIFIED (paper-first gate).
}

/// Peer-triggered soak: A->B handoff with zero orchestrator dispatch.
#[test]
#[ignore = "needs herdr --session test-collab + a test project; run with --ignored once infra lands"]
fn peer_triggered_soak_a_wakes_b() {
    // Placeholder (subtask 4): scripted peer-aware A; assert A fetch_inbox
    // (since_ts incremental), replies+acks, prompts B, B completes. Assert no
    // pane spawn/close by peers (safety) and zero orchestrator dispatch.
}

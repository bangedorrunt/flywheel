//! End-to-end integration and formal verification test suite for hierarchical
//! statecharts (`SwarmLifecycle`, `P2PNegotiation`), CLI `workflow verify`,
//! and lateral P2P protocol coordination (ADR-0020, ADR-0021).
//!
//! Enforces:
//! - Hermetic test execution (temp HOME/XDG, zero ambient state)
//! - Zero-divergence formal statechart checks (reachability, quiescence, deadlock freedom)
//! - AST-derived TLA+ specification generation
//! - CLI JSON schema contracts (`schema: flywheel-workflow-verify.v1`)
//! - P2P multiplayer protocol directive execution

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output};

fn flywheel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

struct HermeticEnv {
    #[expect(dead_code, reason = "keeps tempdir alive until dropped")]
    dir: tempfile::TempDir,
    home: PathBuf,
    config_home: PathBuf,
    state_home: PathBuf,
    data_home: PathBuf,
    runtime_dir: PathBuf,
    toron_data_dir: PathBuf,
}

impl HermeticEnv {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("create temp dir for hermetic test env");
        let home = dir.path().join("home");
        let config_home = dir.path().join("config");
        let state_home = dir.path().join("state");
        let data_home = dir.path().join("data");
        let runtime_dir = dir.path().join("runtime");
        let toron_data_dir = dir.path().join("toron");

        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::create_dir_all(&config_home).expect("mkdir config");
        std::fs::create_dir_all(&state_home).expect("mkdir state");
        std::fs::create_dir_all(&data_home).expect("mkdir data");
        std::fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
        std::fs::create_dir_all(&toron_data_dir).expect("mkdir toron");

        Self {
            dir,
            home,
            config_home,
            state_home,
            data_home,
            runtime_dir,
            toron_data_dir,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(flywheel_bin())
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("TORON_DATA_DIR", &self.toron_data_dir)
            .output()
            .expect("spawn flywheel command")
    }
}

fn stdout_str(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("valid utf8 stdout")
}

fn stderr_str(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("valid utf8 stderr")
}

fn stdout_json(out: &Output) -> serde_json::Value {
    let s = stdout_str(out);
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("stdout not json: {e}\nstdout: {s}"))
}

#[test]
fn test_cli_workflow_verify_plain_text() {
    let env = HermeticEnv::new();
    let out = env.run(&["workflow", "verify"]);
    assert!(
        out.status.success(),
        "workflow verify exited with failure: {}",
        stderr_str(&out)
    );

    let text = stdout_str(&out);
    assert!(
        text.contains("workflow verify SwarmLifecycle: ok"),
        "expected SwarmLifecycle ok in output: {text}"
    );
    assert!(
        text.contains("workflow verify P2PNegotiation: ok"),
        "expected P2PNegotiation ok in output: {text}"
    );
}

#[test]
fn test_cli_workflow_verify_json_contract() {
    let env = HermeticEnv::new();
    let out = env.run(&["workflow", "verify", "--json"]);
    assert!(
        out.status.success(),
        "workflow verify --json failed: {}",
        stderr_str(&out)
    );

    let json = stdout_json(&out);
    assert_eq!(
        json["schema"], "flywheel-workflow-verify.v1",
        "schema contract must match"
    );
    assert_eq!(json["ok"], true, "overall verify verdict must be true");

    let charts = json["charts"]
        .as_array()
        .expect("charts must be a JSON array");
    assert_eq!(charts.len(), 2, "must verify exactly 2 statecharts");

    let chart_names: Vec<&str> = charts
        .iter()
        .map(|c| c["name"].as_str().expect("name must be string"))
        .collect();
    assert!(chart_names.contains(&"SwarmLifecycle"));
    assert!(chart_names.contains(&"P2PNegotiation"));

    for chart in charts {
        assert_eq!(
            chart["ok"], true,
            "chart {} must pass verification",
            chart["name"]
        );
        let checks = &chart["checks"];
        assert_eq!(
            checks["reachability"]["ok"], true,
            "reachability check must pass for {}",
            chart["name"]
        );
        assert_eq!(
            checks["quiescence"]["ok"], true,
            "quiescence check must pass for {}",
            chart["name"]
        );
        assert_eq!(
            checks["deadlock_free_non_final"]["ok"], true,
            "deadlock-free check must pass for {}",
            chart["name"]
        );
    }
}

#[test]
fn test_cli_workflow_verify_single_named_chart() {
    let env = HermeticEnv::new();

    // Verify SwarmLifecycle alone
    let out = env.run(&["workflow", "verify", "SwarmLifecycle", "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["ok"], true);
    let charts = json["charts"].as_array().unwrap();
    assert_eq!(charts.len(), 1);
    assert_eq!(charts[0]["name"], "SwarmLifecycle");

    // Verify P2PNegotiation alone
    let out = env.run(&["workflow", "verify", "P2PNegotiation", "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["ok"], true);
    let charts = json["charts"].as_array().unwrap();
    assert_eq!(charts.len(), 1);
    assert_eq!(charts[0]["name"], "P2PNegotiation");
}

#[test]
fn test_cli_workflow_verify_unknown_chart_fail_stop() {
    let env = HermeticEnv::new();
    let out = env.run(&["workflow", "verify", "NonExistentChartName"]);
    assert!(
        !out.status.success(),
        "workflow verify must fail for unknown chart"
    );
    let err = stderr_str(&out);
    assert!(
        err.contains("unknown statechart NonExistentChartName"),
        "stderr should cite unknown chart: {err}"
    );
}

#[test]
fn test_cli_workflow_verify_export_tla() {
    let env = HermeticEnv::new();
    let out = env.run(&["workflow", "verify", "--export-tla"]);
    assert!(
        out.status.success(),
        "export-tla failed: {}",
        stderr_str(&out)
    );

    let text = stdout_str(&out);
    assert!(
        text.contains("---- MODULE SwarmLifecycle ----"),
        "must contain SwarmLifecycle TLA+ header"
    );
    assert!(
        text.contains("---- MODULE P2PNegotiation ----"),
        "must contain P2PNegotiation TLA+ header"
    );
    assert!(
        text.contains("EXTENDS Naturals"),
        "must declare EXTENDS Naturals"
    );
    assert!(
        text.contains("Init =="),
        "must declare Init state predicate"
    );
    assert!(
        text.contains("Next =="),
        "must declare Next state transition relation"
    );
    assert!(
        text.contains("===="),
        "must terminate with TLA+ module footer"
    );
}

#[test]
fn test_cli_peer_intent_directive() {
    let env = HermeticEnv::new();
    let out = env.run(&[
        "peer",
        "intent",
        "--project",
        "test-proj",
        "--path",
        "src/elm/p2p_negotiation.rs",
        "--scope",
        "unit-test",
        "--bead",
        "bd-test-e2e",
        "--as",
        "AmberHarbor",
    ]);

    assert!(
        out.status.success(),
        "peer intent failed: {}",
        stderr_str(&out)
    );
    let json = stdout_json(&out);
    assert_eq!(json["verb"], "intent");
    assert_eq!(json["sent"], true);
    assert_eq!(json["broadcast"], true);
    assert_eq!(json["project"], "test-proj");
    assert_eq!(json["path"], "src/elm/p2p_negotiation.rs");
}

#[test]
fn test_cli_memory_check_trap_clean() {
    let env = HermeticEnv::new();
    let out = env.run(&[
        "memory",
        "check-trap",
        "unique_clean_path_zero_traps_998877",
    ]);
    assert!(
        out.status.success(),
        "memory check-trap must exit 0: {}",
        stderr_str(&out)
    );
    let text = stdout_str(&out);
    assert!(
        text.contains("no traps"),
        "expected 'no traps' output: {text}"
    );
}

// ---------------------------------------------------------------------------
// Behavioral & Metamorphic Statechart Properties (Zero Ambient I/O)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SwarmPhase {
    Classify,
    Dispatch,
    Execute,
    Review,
    Verify,
    MitigatingTrap,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SwarmEvent {
    DigestReady,
    StepDispatched,
    ReviewRequested,
    ReviewPassed,
    ReviewRejected,
    VerificationPassed,
    TrapDetected,
    TrapMitigated,
}

fn simulate_swarm_step(current: SwarmPhase, event: &SwarmEvent) -> SwarmPhase {
    match (current, event) {
        (SwarmPhase::Classify, SwarmEvent::DigestReady) => SwarmPhase::Dispatch,
        (SwarmPhase::Classify, SwarmEvent::TrapDetected) => SwarmPhase::MitigatingTrap,
        (SwarmPhase::MitigatingTrap, SwarmEvent::TrapMitigated) => SwarmPhase::Dispatch,
        (SwarmPhase::Dispatch, SwarmEvent::StepDispatched) => SwarmPhase::Execute,
        (SwarmPhase::Execute, SwarmEvent::ReviewRequested) => SwarmPhase::Review,
        (SwarmPhase::Review, SwarmEvent::ReviewPassed) => SwarmPhase::Verify,
        (SwarmPhase::Review, SwarmEvent::ReviewRejected) => SwarmPhase::Failed,
        (SwarmPhase::Verify, SwarmEvent::VerificationPassed) => SwarmPhase::Complete,
        (SwarmPhase::Verify, SwarmEvent::TrapDetected) => SwarmPhase::MitigatingTrap,
        _ => current, // Stuttering / no-op for out-of-phase events
    }
}

#[test]
fn test_swarm_lifecycle_full_happy_path() {
    let events = [
        SwarmEvent::DigestReady,
        SwarmEvent::StepDispatched,
        SwarmEvent::ReviewRequested,
        SwarmEvent::ReviewPassed,
        SwarmEvent::VerificationPassed,
    ];

    let mut state = SwarmPhase::Classify;
    let mut history = vec![state];
    for ev in &events {
        state = simulate_swarm_step(state, ev);
        history.push(state);
    }

    assert_eq!(
        history,
        vec![
            SwarmPhase::Classify,
            SwarmPhase::Dispatch,
            SwarmPhase::Execute,
            SwarmPhase::Review,
            SwarmPhase::Verify,
            SwarmPhase::Complete,
        ],
        "happy path must traverse canonical phase ladder"
    );
}

#[test]
fn test_swarm_lifecycle_trap_mitigation_loop() {
    let events = [
        SwarmEvent::DigestReady,
        SwarmEvent::StepDispatched,
        SwarmEvent::ReviewRequested,
        SwarmEvent::ReviewPassed,
        SwarmEvent::TrapDetected,  // Trap discovered during Verify
        SwarmEvent::TrapMitigated, // Mitigated -> re-dispatches to Execute
        SwarmEvent::StepDispatched,
        SwarmEvent::ReviewRequested,
        SwarmEvent::ReviewPassed,
        SwarmEvent::VerificationPassed,
    ];

    let mut state = SwarmPhase::Classify;
    for ev in &events {
        state = simulate_swarm_step(state, ev);
    }
    assert_eq!(
        state,
        SwarmPhase::Complete,
        "lifecycle must recover from trap mitigation into complete state"
    );
}

#[test]
fn test_swarm_lifecycle_review_rejection_fail_stop() {
    let events = [
        SwarmEvent::DigestReady,
        SwarmEvent::StepDispatched,
        SwarmEvent::ReviewRequested,
        SwarmEvent::ReviewRejected,
    ];

    let mut state = SwarmPhase::Classify;
    for ev in &events {
        state = simulate_swarm_step(state, ev);
    }
    assert_eq!(state, SwarmPhase::Failed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum P2PState {
    Idle,
    Proposed,
    WaitingDecision,
    Accepted,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum P2PAction {
    Propose,
    AskCaptain,
    Accept,
    Reject,
    Timeout,
    Reclaim,
}

fn simulate_p2p_step(current: P2PState, action: &P2PAction) -> P2PState {
    match (current, action) {
        (P2PState::Idle, P2PAction::Propose) => P2PState::Proposed,
        (P2PState::Proposed, P2PAction::AskCaptain) => P2PState::WaitingDecision,
        (P2PState::Proposed, P2PAction::Accept) => P2PState::Accepted,
        (P2PState::Proposed, P2PAction::Reject) => P2PState::Rejected,
        (P2PState::Proposed, P2PAction::Timeout) => P2PState::Expired,
        (P2PState::WaitingDecision, P2PAction::Accept) => P2PState::Accepted,
        (P2PState::WaitingDecision, P2PAction::Reject) => P2PState::Rejected,
        (P2PState::WaitingDecision, P2PAction::Timeout) => P2PState::Expired,
        (P2PState::Accepted, P2PAction::Reclaim) => P2PState::Idle,
        (P2PState::Rejected, P2PAction::Reclaim) => P2PState::Idle,
        (P2PState::Expired, P2PAction::Reclaim) => P2PState::Idle,
        _ => current,
    }
}

#[test]
fn test_p2p_negotiation_direct_acceptance() {
    let mut state = P2PState::Idle;
    state = simulate_p2p_step(state, &P2PAction::Propose);
    assert_eq!(state, P2PState::Proposed);
    state = simulate_p2p_step(state, &P2PAction::Accept);
    assert_eq!(state, P2PState::Accepted);
    state = simulate_p2p_step(state, &P2PAction::Reclaim);
    assert_eq!(state, P2PState::Idle);
}

#[test]
fn test_p2p_negotiation_timeout_escalation() {
    let mut state = P2PState::Idle;
    state = simulate_p2p_step(state, &P2PAction::Propose);
    state = simulate_p2p_step(state, &P2PAction::AskCaptain);
    assert_eq!(state, P2PState::WaitingDecision);
    state = simulate_p2p_step(state, &P2PAction::Timeout);
    assert_eq!(state, P2PState::Expired);
    state = simulate_p2p_step(state, &P2PAction::Reclaim);
    assert_eq!(state, P2PState::Idle);
}

#[test]
fn test_p2p_state_space_reachability_and_quiescence() {
    let all_states = [
        P2PState::Idle,
        P2PState::Proposed,
        P2PState::WaitingDecision,
        P2PState::Accepted,
        P2PState::Rejected,
        P2PState::Expired,
    ];

    let all_actions = [
        P2PAction::Propose,
        P2PAction::AskCaptain,
        P2PAction::Accept,
        P2PAction::Reject,
        P2PAction::Timeout,
        P2PAction::Reclaim,
    ];

    // BFS reachability from Idle
    let mut reachable = BTreeSet::new();
    let mut queue = vec![P2PState::Idle];
    reachable.insert(P2PState::Idle);

    while let Some(st) = queue.pop() {
        for act in &all_actions {
            let next = simulate_p2p_step(st, act);
            if reachable.insert(next) {
                queue.push(next);
            }
        }
    }

    for st in &all_states {
        assert!(
            reachable.contains(st),
            "State {:?} must be reachable from Idle",
            st
        );
    }

    // Deadlock freedom: from every non-Idle state, there exists a path back to Idle
    for &start in &all_states {
        let mut visited = BTreeSet::new();
        let mut q = vec![start];
        visited.insert(start);
        let mut can_reach_idle = false;

        while let Some(s) = q.pop() {
            if s == P2PState::Idle {
                can_reach_idle = true;
                break;
            }
            for act in &all_actions {
                let nxt = simulate_p2p_step(s, act);
                if visited.insert(nxt) {
                    q.push(nxt);
                }
            }
        }

        assert!(
            can_reach_idle,
            "State {:?} must be able to return to Idle (quiescent closure)",
            start
        );
    }
}

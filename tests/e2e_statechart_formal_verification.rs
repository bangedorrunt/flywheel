//! CLI contract of `flywheel workflow verify` (`flywheel-workflow-verify.v2`,
//! ADR-0021 D5) and the peer/memory verbs, run against the built binary in a
//! hermetic HOME/XDG. Chart behaviour is tested beside the production charts
//! in `src/elm/`; this file has no copy of their transitions.

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
        text.contains("workflow verify scope: topology only"),
        "plain output must state what is not checked: {text}"
    );
    assert!(
        !text.contains("workflow verify SwarmLifecycle"),
        "the hand-written SwarmLifecycle chart is retired; wave-swarm verifies as a workflow: {text}"
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
        json["schema"], "flywheel-workflow-verify.v2",
        "schema contract must match"
    );
    assert_eq!(json["ok"], true, "overall verify verdict must be true");
    assert!(
        json["scope"]
            .as_str()
            .is_some_and(|s| s.contains("no model checker runs")),
        "the report must say what it does not prove: {}",
        json["scope"]
    );

    let charts = json["charts"]
        .as_array()
        .expect("charts must be a JSON array");
    let chart_names: Vec<&str> = charts
        .iter()
        .map(|c| c["name"].as_str().expect("name must be string"))
        .collect();
    assert!(
        !chart_names.contains(&"SwarmLifecycle"),
        "the hand-written SwarmLifecycle chart is retired: {chart_names:?}"
    );
    assert!(chart_names.contains(&"P2PNegotiation"));
    assert!(
        chart_names.contains(&"wave-swarm"),
        "wave-swarm verifies as the lowered workflow: {chart_names:?}"
    );

    for chart in charts {
        assert_eq!(
            chart["ok"], true,
            "chart {} must pass verification",
            chart["name"]
        );
        let checks = &chart["checks"];
        assert_eq!(
            checks["reachable"]["unreachable"],
            serde_json::json!([]),
            "every state of {} must be reachable",
            chart["name"]
        );
        assert_eq!(
            checks["can_finish"]["stuck"],
            serde_json::json!([]),
            "every reachable leaf of {} must reach its goal",
            chart["name"]
        );
    }
}

#[test]
fn test_cli_workflow_verify_single_named_chart() {
    let env = HermeticEnv::new();

    // The retired chart is no longer addressable (wave-swarm is a workflow).
    let out = env.run(&["workflow", "verify", "SwarmLifecycle", "--json"]);
    assert!(!out.status.success(), "the retired chart must not verify");

    let out = env.run(&["workflow", "verify", "wave-swarm", "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["ok"], true);
    let charts = json["charts"].as_array().unwrap();
    assert_eq!(charts.len(), 1);
    assert_eq!(charts[0]["name"], "wave-swarm");

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
        !text.contains("---- MODULE SwarmLifecycle ----"),
        "the retired chart exports no module"
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

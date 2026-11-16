//! flywheel decide CLI — clap parse + `--json` shape (bd-irrt).
//!
//! `flywheel decide <target> [--ready-beads N] [--profiled-panes N]
//! [--launch-pending N] [--contention] [--project P] [--json]` classifies a captain
//! target into ONE primitive and prints
//! `{primitive, reason, argv, topology, role}`.
//! Never dispatches. Exit 0. Absent snapshot facts stay zero/false.
//!
//! bd-2o1v added `role` (the routed workflow's default profile, `-`/null when
//! the row dispatches to none); these tests were left pinning the pre-`role`
//! shape and were red on HEAD.
//!
//! The classifier itself is unit-tested in `src/decide.rs` (bd-5inr);
//! these tests pin the CLI contract: clap parse, JSON shape, plain lines,
//! exit codes.
//!
//! governed-by: ADR-0005 (orchestration vocabulary)

use std::process::Output;

use flywheel_test_sandbox::TestSandbox;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

fn run(sandbox: &TestSandbox, args: &[&str]) -> Output {
    sandbox
        .command(flywheel())
        .args(args)
        .output()
        .expect("spawn flywheel")
}

fn stdout_json(out: &Output) -> serde_json::Value {
    let s = String::from_utf8(out.stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(&s).expect("stdout parses as JSON")
}

#[test]
fn decide_audit_json_shape() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "audit the mailbox layer", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "captain-audit");
    assert_eq!(v["topology"], "open");
    assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()));
    assert_eq!(
        v["argv"],
        serde_json::json!([
            "flywheel",
            "workflow",
            "run",
            "captain-audit",
            "--execute",
            "--mode",
            "headless",
        ])
    );
}

#[test]
fn decide_ready_beads_is_loop_work() {
    let sandbox = TestSandbox::new();
    let out = run(
        &sandbox,
        &[
            "decide",
            "work the ready beads",
            "--ready-beads",
            "3",
            "--json",
        ],
    );
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "loop-work");
    let argv = v["argv"].as_array().expect("argv is array");
    assert!(argv.windows(2).any(|w| w == ["loop-mode", "run"]));
    assert!(argv.iter().any(|a| a == "<project>"));
    assert!(argv.iter().any(|a| a == "--work"));
}

#[test]
fn decide_implement_is_steer() {
    let sandbox = TestSandbox::new();
    let out = run(
        &sandbox,
        &["decide", "implement the parser with a plan gate", "--json"],
    );
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "steer");
    assert_eq!(
        v["argv"],
        serde_json::json!(["flywheel", "steer", "implement the parser with a plan gate"])
    );
}

/// ADR-0019 Spec 8 !implement guard, CLI surface: `fix the memory leak` is a
/// BUG-FIX (repro→red→fix→prove playbook), never forensics — implement-bearing
/// targets must not fall to a diagnosis-only workflow even when they carry a
/// forensics-shaped needle (`leak`).
#[test]
fn decide_fix_memory_leak_is_bug_fix_not_forensics() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "fix the memory leak", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "bug-fix");
    assert_eq!(
        v["argv"],
        serde_json::json!([
            "flywheel",
            "workflow",
            "run",
            "bug-fix",
            "--execute",
            "--mode",
            "pane",
        ])
    );
}

/// ADR-0019 Spec 8 !implement guard, forensics side: `profile the idle cpu
/// spin` is FORENSICS (diagnosis is the artifact, no fix step) — the
/// forensics row is read-only and an implement-bearing target never reaches
/// it.
#[test]
fn decide_profile_idle_cpu_spin_is_forensics() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "profile the idle cpu spin", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "forensics");
    assert_eq!(
        v["argv"],
        serde_json::json!([
            "flywheel",
            "workflow",
            "run",
            "forensics",
            "--execute",
            "--mode",
            "headless",
        ])
    );
}

#[test]
fn decide_contention_flag_is_coalition() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "ship it", "--contention", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "coalition-form");
}

#[test]
fn decide_plain_mode_prints_contract_lines() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "analyze the durable loop"]);
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).expect("utf8 stdout");
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(
        lines.len(),
        5,
        "primitive / reason / argv / topology / role lines: {s}"
    );
    assert_eq!(lines[0], "captain-audit");
    assert!(lines[1].contains("read-only"));
    assert!(lines[2].starts_with("flywheel workflow run captain-audit"));
    assert_eq!(lines[3], "topology open");
    assert_eq!(lines[4], "role -", "role line must be explicit: {s}");
}

#[test]
fn decide_unknown_target_is_explicit_none() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "zzz fuzzy intent", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "none");
    assert_eq!(v["topology"], "none");
    assert_eq!(v["argv"], serde_json::json!([]));
    assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()));
}

#[test]
fn decide_rejects_unknown_flag_usage_error() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "audit", "--nonsense"]);
    assert_eq!(out.status.code(), Some(2), "clap usage error exit code");
}

#[test]
fn decide_json_never_dispatches() {
    let sandbox = TestSandbox::new();
    let out = run(&sandbox, &["decide", "implement the parser", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    let mut keys: Vec<&str> = v
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["argv", "primitive", "reason", "role", "topology"]
    );
    for forbidden in [
        "dispatch",
        "dispatch_job",
        "cmd",
        "args",
        "exec",
        "spawn",
        "run",
    ] {
        assert!(
            !v.as_object().expect("object").contains_key(forbidden),
            "decide --json grew a side-effect field `{forbidden}`: {v}"
        );
    }
    assert!(v["argv"].is_array(), "argv is the only actionable output");
    for inert in ["primitive", "reason", "role", "topology"] {
        let field = &v[inert];
        assert!(
            field.is_string() || field.is_null(),
            "{inert} must stay inert metadata, got {field}"
        );
    }
}

//! flywheel decide CLI — clap parse + `--json` shape (bd-irrt).
//!
//! `flywheel decide <target> [--ready-beads N] [--profiled-panes N]
//! [--launch-pending N] [--contention] [--json]` classifies a captain
//! target into ONE primitive and prints `{primitive, reason, argv}`.
//! Never dispatches. Exit 0. Absent snapshot facts stay zero/false.
//!
//! The classifier itself is unit-tested in `src/decide.rs` (bd-5inr);
//! these tests pin the CLI contract: clap parse, JSON shape, plain lines,
//! exit codes.
//!
//! governed-by: ADR-0005 (orchestration vocabulary)

use std::path::PathBuf;
use std::process::{Command, Output};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

struct Hermetic {
    #[expect(
        dead_code,
        reason = "keeps the temp dir alive for the hermetic env's lifetime"
    )]
    dir: tempfile::TempDir,
    config_home: PathBuf,
    state_home: PathBuf,
}

fn hermetic() -> Hermetic {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config_home = dir.path().join("config");
    let state_home = dir.path().join("state");
    Hermetic {
        dir,
        config_home,
        state_home,
    }
}

fn run(h: &Hermetic, args: &[&str]) -> Output {
    Command::new(flywheel())
        .args(args)
        .env("XDG_CONFIG_HOME", &h.config_home)
        .env("XDG_STATE_HOME", &h.state_home)
        .output()
        .expect("spawn flywheel")
}

fn stdout_json(out: &Output) -> serde_json::Value {
    let s = String::from_utf8(out.stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(&s).expect("stdout parses as JSON")
}

#[test]
fn decide_audit_json_shape() {
    let h = hermetic();
    let out = run(&h, &["decide", "audit the mailbox layer", "--json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "captain-audit");
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
    let h = hermetic();
    let out = run(
        &h,
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
    assert!(argv.windows(2).any(|w| w == ["loop", "run"]));
    assert!(argv.iter().any(|a| a == "--work"));
}

#[test]
fn decide_implement_is_steer() {
    let h = hermetic();
    let out = run(
        &h,
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

#[test]
fn decide_contention_flag_is_coalition() {
    let h = hermetic();
    let out = run(&h, &["decide", "ship it", "--contention", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "coalition-form");
}

#[test]
fn decide_plain_mode_prints_three_lines() {
    let h = hermetic();
    let out = run(&h, &["decide", "analyze the durable loop"]);
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).expect("utf8 stdout");
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 3, "primitive / reason / argv lines: {s}");
    assert_eq!(lines[0], "captain-audit");
    assert!(lines[1].contains("read-only"));
    assert!(lines[2].starts_with("flywheel workflow run captain-audit"));
}

#[test]
fn decide_exit_zero_on_unknown_target_defaults_steer() {
    let h = hermetic();
    let out = run(&h, &["decide", "zzz fuzzy intent", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    assert_eq!(v["primitive"], "steer");
    assert_eq!(
        v["argv"],
        serde_json::json!(["flywheel", "steer", "zzz fuzzy intent"])
    );
}

#[test]
fn decide_rejects_unknown_flag_usage_error() {
    let h = hermetic();
    let out = run(&h, &["decide", "audit", "--nonsense"]);
    assert_eq!(out.status.code(), Some(2), "clap usage error exit code");
}

#[test]
fn decide_json_never_dispatches() {
    // The JSON contract must never grow a side-effect field: argv is the
    // ONLY actionable output, and stdout is pure JSON (no logs on stderr
    // path pollution).
    let h = hermetic();
    let out = run(&h, &["decide", "implement the parser", "--json"]);
    assert!(out.status.success());
    let v = stdout_json(&out);
    let mut keys: Vec<&str> = v
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["argv", "primitive", "reason"]);
}

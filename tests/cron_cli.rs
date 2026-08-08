//! flywheel cron CLI — hermetic CLI-level tests (ADR-0005 Spec 3 / P4.6).
//!
//! Spawns the built binary against a temp XDG_CONFIG_HOME so the registry
//! (`crons.toml`), the workflow catalog (`workflows/*.yaml`) and profiles
//! (`profiles.toml`) all resolve hermetically — no process-global env races.
//!
//! Covers the bead's acceptance:
//! - `flywheel cron list` shows registered crons (+ schedule workflows)
//! - `flywheel cron pause/resume` toggles without losing registration state
//! - `--json` for agent drivers
//! - `add` fail-stop validation (unknown template / unparsable cron)
//!
//! governed-by: ADR-0005 Spec 3

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

struct Hermetic {
    _dir: tempfile::TempDir,
    config_home: PathBuf,
    state_home: PathBuf,
}

fn hermetic() -> Hermetic {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config_home = dir.path().join("config");
    let state_home = dir.path().join("state");
    fs::create_dir_all(config_home.join("flywheel/workflows")).expect("workflows dir");
    // A schedule-triggered workflow that `cron add` can bind a template to.
    fs::write(
        config_home.join("flywheel/workflows/weekly-docker-upgrade.yaml"),
        r##"
name: weekly-docker-upgrade
trigger:
  type: schedule
  cron: "30 3 * * 0"
  owner: housekeeper
  scope: host
steps:
  - name: upgrade
    action: dispatch_job
    assignee: housekeeper
    spec: { profile: "@housekeeper", stage: "systematic-execute" }
"##,
    )
    .expect("workflow yaml");
    // A second, invoke-only workflow must NOT surface in cron list.
    fs::write(
        config_home.join("flywheel/workflows/on-demand.yaml"),
        r##"
name: on-demand
steps:
  - name: plan
    action: send_dm
    to: housekeeper
    body: "on demand"
"##,
    )
    .expect("workflow yaml");
    Hermetic {
        _dir: dir,
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

/// Acceptance: `flywheel cron list` shows registered crons + schedule
/// workflows, and excludes invoke-only workflows.
#[test]
fn cron_list_shows_registered_and_schedule_workflows() {
    let h = hermetic();

    let out = run(&h, &["cron", "add", "weekly-docker-upgrade",
        "--template", "weekly-docker-upgrade",
        "--cron", "30 3 * * 0",
        "--owner", "housekeeper"]);
    assert!(out.status.success(), "add stdout: {}", String::from_utf8_lossy(&out.stdout));
    assert!(out.stderr.is_empty() || !out.status.success());

    let out = run(&h, &["cron", "list", "--json"]);
    assert!(out.status.success(), "list stdout: {}", String::from_utf8_lossy(&out.stdout));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("list json");
    let crons = v["crons"].as_array().expect("crons array");
    let names: Vec<&str> = crons.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"weekly-docker-upgrade"),
        "registered cron in list: {names:?}"
    );
    assert!(
        !names.contains(&"on-demand"),
        "invoke-only workflow excluded: {names:?}"
    );
    // the registered cron's row carries the full spec for agent drivers
    let row = crons.iter().find(|c| c["name"] == "weekly-docker-upgrade").unwrap();
    assert_eq!(row["template"], "weekly-docker-upgrade");
    assert_eq!(row["cron"], "30 3 * * 0");
    assert_eq!(row["owner"], "housekeeper");
    assert_eq!(row["scope"], "host");
    assert_eq!(row["source"], "cron");
    assert!(row["enabled"].as_bool().unwrap());
}

/// Acceptance: pause then resume toggles `enabled` without losing any other
/// registration field (the "without losing state" contract).
#[test]
fn cron_pause_resume_preserves_registration() {
    let h = hermetic();
    let add = run(&h, &["cron", "add", "weekly-docker-upgrade",
        "--template", "weekly-docker-upgrade",
        "--cron", "30 3 * * 0",
        "--owner", "housekeeper"]);
    assert!(add.status.success());

    let pause = run(&h, &["cron", "pause", "weekly-docker-upgrade"]);
    assert!(pause.status.success(), "pause: {}", String::from_utf8_lossy(&pause.stdout));

    let list = run(&h, &["cron", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let row = v["crons"].as_array().unwrap().iter()
        .find(|c| c["name"] == "weekly-docker-upgrade").unwrap();
    assert!(!row["enabled"].as_bool().unwrap(), "paused: enabled=false");
    assert_eq!(row["status"], "paused");
    assert_eq!(row["template"], "weekly-docker-upgrade", "template preserved");
    assert_eq!(row["cron"], "30 3 * * 0", "cron expr preserved");
    assert_eq!(row["owner"], "housekeeper", "owner preserved");
    assert_eq!(row["scope"], "host", "scope preserved");

    let resume = run(&h, &["cron", "resume", "weekly-docker-upgrade"]);
    assert!(resume.status.success());
    let list2 = run(&h, &["cron", "list", "--json"]);
    let v2: serde_json::Value = serde_json::from_slice(&list2.stdout).unwrap();
    let row2 = v2["crons"].as_array().unwrap().iter()
        .find(|c| c["name"] == "weekly-docker-upgrade").unwrap();
    assert!(row2["enabled"].as_bool().unwrap(), "resumed: enabled=true");
    assert_eq!(row2["template"], "weekly-docker-upgrade", "state intact after resume");
}

/// `cron add` fails closed on an unresolvable template.
#[test]
fn cron_add_rejects_unknown_template() {
    let h = hermetic();
    let out = run(&h, &["cron", "add", "nope",
        "--template", "does-not-exist",
        "--cron", "30 3 * * 0",
        "--owner", "housekeeper"]);
    assert!(!out.status.success(), "unknown template must fail");
}

/// `cron add` fails closed on an unparsable cron expression.
#[test]
fn cron_add_rejects_invalid_cron() {
    let h = hermetic();
    let out = run(&h, &["cron", "add", "bad",
        "--template", "weekly-docker-upgrade",
        "--cron", "not a cron",
        "--owner", "housekeeper"]);
    assert!(!out.status.success(), "invalid cron expr must fail");
}

/// `cron add` fails closed on an unresolvable owner profile.
#[test]
fn cron_add_rejects_unknown_owner() {
    let h = hermetic();
    let out = run(&h, &["cron", "add", "badowner",
        "--template", "weekly-docker-upgrade",
        "--cron", "30 3 * * 0",
        "--owner", "no-such-profile"]);
    assert!(!out.status.success(), "unknown owner must fail");
}

/// `cron status` surfaces every cron with a derived status (JSON shape).
#[test]
fn cron_status_lists_all_with_derived_status() {
    let h = hermetic();
    let add = run(&h, &["cron", "add", "weekly-docker-upgrade",
        "--template", "weekly-docker-upgrade",
        "--cron", "30 3 * * 0",
        "--owner", "housekeeper"]);
    assert!(add.status.success());

    let out = run(&h, &["cron", "status", "--json"]);
    assert!(out.status.success(), "status: {}", String::from_utf8_lossy(&out.stdout));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v["crons"].as_array().unwrap();
    assert!(!rows.is_empty());
    let row = rows.iter().find(|c| c["name"] == "weekly-docker-upgrade").unwrap();
    // fresh cron with no runs → due
    assert_eq!(row["status"], "due");
    assert!(row["next_due"].as_i64().is_some(), "next_due computed");
}

/// `cron history` on a cron with no recorded runs returns an empty list (JSON).
#[test]
fn cron_history_empty_runs_json() {
    let h = hermetic();
    let add = run(&h, &["cron", "add", "weekly-docker-upgrade",
        "--template", "weekly-docker-upgrade",
        "--cron", "30 3 * * 0",
        "--owner", "housekeeper"]);
    assert!(add.status.success());

    let out = run(&h, &["cron", "history", "weekly-docker-upgrade", "--json"]);
    assert!(out.status.success(), "history: {}", String::from_utf8_lossy(&out.stdout));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["cron"], "weekly-docker-upgrade");
    assert_eq!(v["runs"].as_array().unwrap().len(), 0);
}

/// The vixie day-of-week `0` (Sunday) round-trips through add→show.
#[test]
fn cron_add_accepts_vixie_sunday_zero() {
    let h = hermetic();
    let add = run(&h, &["cron", "add", "sunday-cron",
        "--template", "weekly-docker-upgrade",
        "--cron", "0 2 * * 0",
        "--owner", "housekeeper"]);
    assert!(add.status.success(), "dow=0 must be accepted: {}", String::from_utf8_lossy(&add.stdout));
    let out = run(&h, &["cron", "show", "sunday-cron", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["cron"], "0 2 * * 0");
    assert_eq!(v["status"], "due");
}

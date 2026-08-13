//! flywheel cron CLI — hermetic CLI-level tests (ADR-0012 Spec 1/4, P12.3).
//!
//! Spawns the built binary against a temp XDG_CONFIG_HOME + a temp project
//! dir so the cron YAML tiers (global `~/.config/flywheel/crons/` +
//! per-project `<project>/.flywheel/crons/`) resolve hermetically — no
//! process-global env races.
//!
//! Covers the bead's gate:
//! - `flywheel cron lint/run/list/show/pause/resume/history/remove` work over
//!   cron YAMLs
//! - a per-project cron shadows a global same-name shadows a built-in
//! - `cron list` shows the resolved source + scope
//! - `cron lint` rejects an invalid schedule/owner
//!
//! governed-by: ADR-0005 Spec 3, ADR-0012 Spec 4

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

struct Hermetic {
    #[expect(dead_code, reason = "keeps the temp dir alive for the hermetic env's lifetime")]
    dir: tempfile::TempDir,
    config_home: PathBuf,
    state_home: PathBuf,
    /// The per-project tier root (passed as `--project <abs>` so cwd never
    /// leaks into the resolution).
    project: PathBuf,
}

fn cron_yaml(name: &str, owner: &str, schedule: &str) -> String {
    format!(
        "name: {name}\nschedule: \"{schedule}\"\nowner: {owner}\nscope: host\nsteps:\n  - name: noop\n    action: delay\n    secs: 0\n"
    )
}

fn hermetic() -> Hermetic {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config_home = dir.path().join("config");
    let state_home = dir.path().join("state");
    let project = dir.path().join("proj");
    Hermetic {
        dir,
        config_home,
        state_home,
        project,
    }
}

fn global_dir(h: &Hermetic) -> PathBuf {
    h.config_home.join("flywheel/crons")
}

fn project_dir(h: &Hermetic) -> PathBuf {
    h.project.join(".flywheel/crons")
}

fn write_global(h: &Hermetic, name: &str, owner: &str, schedule: &str) {
    let dir = global_dir(h);
    fs::create_dir_all(&dir).expect("global crons dir");
    fs::write(dir.join(format!("{name}.yaml")), cron_yaml(name, owner, schedule))
        .expect("global cron yaml");
}

fn write_project(h: &Hermetic, name: &str, owner: &str, schedule: &str) {
    let dir = project_dir(h);
    fs::create_dir_all(&dir).expect("project crons dir");
    fs::write(dir.join(format!("{name}.yaml")), cron_yaml(name, owner, schedule))
        .expect("project cron yaml");
}

fn run(h: &Hermetic, args: &[&str]) -> Output {
    Command::new(flywheel())
        .args(args)
        .env("XDG_CONFIG_HOME", &h.config_home)
        .env("XDG_STATE_HOME", &h.state_home)
        .output()
        .expect("spawn flywheel")
}

fn project_flag(h: &Hermetic) -> String {
    format!("--project={}", h.project.display())
}

fn json(out: &Output) -> serde_json::Value {
    assert!(out.status.success(), "stdout: {}", String::from_utf8_lossy(&out.stdout));
    serde_json::from_slice(&out.stdout).expect("valid json on stdout")
}

/// Gate: `cron list` resolves the tiers with precedence project > global >
/// built-in and shows source + scope. The built-in tier is `weekly-review`
/// (compiled-in schedule-triggered workflow, owner oracle, scope project).
#[test]
fn list_precedence_project_shadows_global_shadows_builtin() {
    let h = hermetic();

    // global tier only: alpha (new) + weekly-review (shadows the built-in)
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    write_global(&h, "weekly-review", "housekeeper", "0 4 * * 0");

    // no project tier: global wins over built-in; built-in shows only when
    // no user tier defines it
    let v = json(&run(&h, &["cron", "list", "--json", "--project", "/nonexistent-proj"]));
    let by_name = |n: &str| {
        v["crons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .expect(n)
            .clone()
    };
    assert_eq!(by_name("alpha")["source"], "global");
    assert_eq!(by_name("alpha")["owner"], "housekeeper");
    assert_eq!(by_name("weekly-review")["source"], "global", "global shadows builtin");
    assert_eq!(by_name("weekly-review")["owner"], "housekeeper");

    // project tier shadows global + built-in
    write_project(&h, "alpha", "scout", "0 2 * * 0");
    write_project(&h, "weekly-review", "scout", "0 4 * * 0");
    let v = json(&run(&h, &["cron", "list", "--json", &project_flag(&h)]));
    let by_name = |n: &str| {
        v["crons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .expect(n)
            .clone()
    };
    assert_eq!(by_name("alpha")["source"], "project", "project shadows global");
    assert_eq!(by_name("alpha")["owner"], "scout");
    assert_eq!(by_name("weekly-review")["source"], "project", "project shadows global+builtin");
    assert_eq!(by_name("weekly-review")["owner"], "scout");

    // bare built-in: no user tiers at all -> compiled-in weekly-review
    let v = json(&run(&h, &["cron", "list", "--json",
        "--project", "/nonexistent-proj2"]));
    // NOTE: global tier was written in this test — use a fresh hermetic env
    // for the pure-builtin case instead.
    let _ = v;
    let h2 = hermetic();
    let v = json(&run(&h2, &["cron", "list", "--json", "--project", "/nonexistent-proj2"]));
    let wr = v["crons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "weekly-review")
        .expect("builtin weekly-review");
    assert_eq!(wr["source"], "builtin");
    assert_eq!(wr["owner"], "oracle");
    assert_eq!(wr["scope"], "project", "builtin scope carried from the workflow");
}

/// Gate: `cron list` shows the resolved source + scope on every row.
#[test]
fn list_rows_carry_source_and_scope() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    let v = json(&run(&h, &["cron", "list", "--json", "--project", "/nonexistent-proj"]));
    let row = v["crons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "alpha")
        .expect("alpha");
    assert_eq!(row["source"], "global");
    assert_eq!(row["scope"], "host");
    assert_eq!(row["schedule"], "30 3 * * 0");
    assert_eq!(row["owner"], "housekeeper");
    assert_eq!(row["status"], "due");
    assert!(row["next_due"].as_i64().is_some(), "next_due computed");
    assert_eq!(row["mode"], "headless");
    assert!(row["enabled"].as_bool().unwrap());
}

/// Gate: `cron show` prints the resolved definition (spec + source + scope).
#[test]
fn show_resolves_definition() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    let v = json(&run(&h, &["cron", "show", "alpha", "--json", "--project", "/nonexistent-proj"]));
    assert_eq!(v["name"], "alpha");
    assert_eq!(v["source"], "global");
    assert_eq!(v["scope"], "host");
    assert_eq!(v["schedule"], "30 3 * * 0");
    assert_eq!(v["owner"], "housekeeper");
    assert_eq!(v["status"], "due");

    let out = run(&h, &["cron", "show", "nope", "--json", "--project", "/nonexistent-proj"]);
    assert!(!out.status.success(), "unknown cron fails");
}

/// Gate: pause/resume toggles `enabled` in the tier's YAML without losing
/// the definition (the pause/resume contract).
#[test]
fn pause_resume_persists_in_yaml_and_preserves_state() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    let path = global_dir(&h).join("alpha.yaml");

    let pause = run(&h, &["cron", "pause", "alpha", "--project", "/nonexistent-proj"]);
    assert!(pause.status.success(), "pause: {}", String::from_utf8_lossy(&pause.stdout));
    let raw = fs::read_to_string(&path).expect("alpha.yaml");
    assert!(raw.contains("enabled: false"), "pause persisted: {raw}");

    let v = json(&run(&h, &["cron", "status", "--json", "--project", "/nonexistent-proj"]));
    let row = v["crons"].as_array().unwrap().iter().find(|c| c["name"] == "alpha").unwrap();
    assert_eq!(row["status"], "paused");
    assert!(!row["enabled"].as_bool().unwrap());

    let resume = run(&h, &["cron", "resume", "alpha", "--project", "/nonexistent-proj"]);
    assert!(resume.status.success());
    let raw = fs::read_to_string(&path).expect("alpha.yaml");
    assert!(!raw.contains("enabled: false"), "resume clears the marker: {raw}");
    assert!(raw.contains("schedule: 30 3 * * 0"), "definition preserved: {raw}");
    assert!(raw.contains("owner: housekeeper"), "owner preserved: {raw}");

    let v = json(&run(&h, &["cron", "status", "--json", "--project", "/nonexistent-proj"]));
    let row = v["crons"].as_array().unwrap().iter().find(|c| c["name"] == "alpha").unwrap();
    assert_eq!(row["status"], "due");
}

/// Pause/resume on a built-in fails (read-only tier).
#[test]
fn pause_builtin_fails() {
    let h = hermetic();
    let out = run(&h, &["cron", "pause", "weekly-review", "--project", "/nonexistent-proj"]);
    assert!(!out.status.success(), "built-in is read-only");
}

/// Gate: remove deletes the tier's YAML; removing a project-tier cron
/// reveals the global definition it shadowed.
#[test]
fn remove_reveals_lower_tier() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    write_project(&h, "alpha", "scout", "0 2 * * 0");

    let rm = run(&h, &["cron", "remove", "alpha", &project_flag(&h)]);
    assert!(rm.status.success(), "remove: {}", String::from_utf8_lossy(&rm.stdout));
    assert!(!project_dir(&h).join("alpha.yaml").exists(), "project yaml deleted");

    let v = json(&run(&h, &["cron", "list", "--json", &project_flag(&h)]));
    let row = v["crons"].as_array().unwrap().iter().find(|c| c["name"] == "alpha").unwrap();
    assert_eq!(row["source"], "global", "project removal reveals global");
    assert_eq!(row["owner"], "housekeeper");

    // remove the global too -> gone entirely
    let rm = run(&h, &["cron", "remove", "alpha", "--project", "/nonexistent-proj"]);
    assert!(rm.status.success());
    let v = json(&run(&h, &["cron", "list", "--json", "--project", "/nonexistent-proj"]));
    assert!(
        v["crons"].as_array().unwrap().iter().all(|c| c["name"] != "alpha"),
        "alpha removed"
    );

    // removing a built-in fails
    let out = run(&h, &["cron", "remove", "weekly-review", "--project", "/nonexistent-proj"]);
    assert!(!out.status.success(), "built-in is read-only");
}

/// Gate: `cron lint` rejects an invalid schedule (exit 1).
#[test]
fn lint_rejects_invalid_schedule() {
    let h = hermetic();
    write_global(&h, "bad-schedule", "housekeeper", "not a cron");
    let out = run(&h, &["cron", "lint", "bad-schedule", "--project", "/nonexistent-proj"]);
    assert_eq!(out.status.code(), Some(1), "invalid schedule must fail lint");
}

/// Gate: `cron lint` rejects an unknown owner profile (flywheel tier).
#[test]
fn lint_rejects_unknown_owner() {
    let h = hermetic();
    write_global(&h, "bad-owner", "no-such-profile", "30 3 * * 0");
    let out = run(&h, &["cron", "lint", "bad-owner", "--project", "/nonexistent-proj"]);
    assert_eq!(out.status.code(), Some(1), "unknown owner must fail lint");
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(msg.contains("does not resolve"), "owner resolution error named: {msg}");
}

/// Gate: a valid cron lints clean (engine tier + owner resolution).
#[test]
fn lint_accepts_valid_cron() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    let out = run(&h, &["cron", "lint", "alpha", "--project", "/nonexistent-proj"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lints clean"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Gate: `cron run` executes the spec body on demand through the shared
/// engine (pane mode = recording runner — no subprocesses, hermetic).
#[test]
fn run_executes_spec_body() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    // force pane mode so the runner records (delay step, no dispatch)
    let path = global_dir(&h).join("alpha.yaml");
    let mut raw = fs::read_to_string(&path).unwrap();
    raw = raw.replace("scope: host\n", "scope: host\nmode: pane\n");
    fs::write(&path, raw).unwrap();

    let out = run(&h, &["cron", "run", "alpha", "--json", "--project", "/nonexistent-proj"]);
    assert!(out.status.success(), "run: {}", String::from_utf8_lossy(&out.stdout));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["cron"], "alpha");
    assert!(v["run_id"].as_str().is_some());
    assert_eq!(v["source"], "global");
}

/// `cron history` on a cron with no recorded runs returns an empty list.
#[test]
fn history_empty_runs_json() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    let v = json(&run(&h, &["cron", "history", "alpha", "--json"]));
    assert_eq!(v["cron"], "alpha");
    assert_eq!(v["runs"].as_array().unwrap().len(), 0);
}

/// `cron status` surfaces every cron with a derived status.
#[test]
fn status_lists_all_with_derived_status() {
    let h = hermetic();
    write_global(&h, "alpha", "housekeeper", "30 3 * * 0");
    let v = json(&run(&h, &["cron", "status", "--json", "--project", "/nonexistent-proj"]));
    let rows = v["crons"].as_array().unwrap();
    assert!(!rows.is_empty());
    let row = rows.iter().find(|c| c["name"] == "alpha").unwrap();
    assert_eq!(row["status"], "due");
    assert!(row["next_due"].as_i64().is_some(), "next_due computed");
}

/// The vixie day-of-week `0` (Sunday) parses through show.
#[test]
fn show_accepts_vixie_sunday_zero() {
    let h = hermetic();
    write_global(&h, "sunday-cron", "housekeeper", "0 2 * * 0");
    let v = json(&run(&h, &["cron", "show", "sunday-cron", "--json", "--project", "/nonexistent-proj"]));
    assert_eq!(v["schedule"], "0 2 * * 0");
    assert_eq!(v["status"], "due");
}

/// Missing cron dirs are EMPTY tiers, never an error.
#[test]
fn empty_tiers_list_empty() {
    let h = hermetic();
    let v = json(&run(&h, &["cron", "list", "--json", "--project", "/nonexistent-proj"]));
    let names: Vec<&str> = v["crons"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["source"] != "builtin")
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.is_empty(), "no user crons: {names:?}");
}

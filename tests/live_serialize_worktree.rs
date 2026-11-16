//! Live: a linked worktree gates `loop run` until the operator opts out.
//!
//! `live_*.rs` carve-out. Scenario B creates a workspace on the REAL hc and
//! runs `loop run` against the operator's live preflight (doorbell, toron mail
//! store under the real data dir, operator key). Isolating it would mean faking
//! all of those, so it is `#[ignore]`d and gated on `FLYWHEEL_LIVE_E2E`.
//!
//! The pure halves are unit-tested in `src/preflight.rs`
//! (`linked_worktrees_filters_primary_tree`, `worktree_optout_legal`), and the
//! always-on CLI surface is in `tests/e2e_serialize_worktree.rs`.
//!
//! governed-by: ADR-0019 Spec 3.3

use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

fn plane_up() -> bool {
    Command::new("hc")
        .args(["workspace", "list"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Engine store reachable? The `--allow-worktree` success half of Scenario B
/// requires ALL preflight checks to pass, including the engine-store check.
fn engine_up() -> bool {
    Command::new(flywheel())
        .args(["preflight", "--mode", "cron", "--json"])
        .output()
        .map(|o| {
            let v: serde_json::Value =
                serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null);
            v["checks"]
                .as_array()
                .map(|cs| {
                    cs.iter()
                        .any(|c| c["name"] == "engine" && c["status"] == "pass")
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// `flywheel loop run <scratch>` in a workspace with an extra `git worktree
/// add` sibling exits NON-ZERO without `--allow-worktree` (preflight FAIL
/// naming the linked tree) and SUCCEEDS with it (operator opt-out LOGGED).
/// Live-gated: needs hc (workspace + worktree list) and the engine store (all
/// preflight checks must pass for the success half). Skips when down.
///
/// Opt-in only (`FLYWHEEL_LIVE_E2E=1`): it creates a workspace on the REAL
/// hc and runs `loop run` against the operator's live preflight (doorbell,
/// toron mail store under the real data dir, operator key). Isolating it
/// would mean faking all of those, so a plain `cargo test` must not reach
/// any of them; the pure halves are unit-tested in `src/preflight.rs`
/// (`linked_worktrees_filters_primary_tree`, `worktree_optout_legal`).
#[ignore = "needs a live hc plane and engine store (FLYWHEEL_LIVE_E2E=1)"]
#[test]
fn linked_worktree_gates_loop_run_until_operator_optout() {
    if !live_e2e_opted_in() {
        eprintln!(
            "{LIVE_E2E} unset — Scenario B skipped (live hc/preflight; CLI surface covered above)"
        );
        return;
    }
    if !plane_up() {
        eprintln!("hc down — Scenario B skipped (CLI surface covered above)");
        return;
    }
    if !engine_up() {
        eprintln!("engine store down — Scenario B skipped (CLI surface covered above)");
        return;
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("fw-zord-live-{}-{ts}", std::process::id()));
    let primary = base.clone();
    std::fs::create_dir_all(&primary).unwrap();

    run_ok(&primary, "git", &["init", "-q"], "git init");
    run_ok(
        &primary,
        "git",
        &[
            "-c",
            "user.email=test@test",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
        "git commit",
    );
    let sibling =
        std::env::temp_dir().join(format!("fw-zord-live-{}-{ts}-sibling", std::process::id()));
    run_ok(
        &primary,
        "git",
        &["worktree", "add", "-q", sibling.to_str().unwrap()],
        "git worktree add sibling",
    );

    let project = primary.to_str().unwrap().to_string();
    let label = format!(
        "flywheel-{}",
        primary.file_name().unwrap().to_str().unwrap()
    );
    let ws = plane_workspace_create(&label, &project);

    let out = Command::new(flywheel())
        .args([
            "loop-mode",
            "run",
            &project,
            "--once",
            "--timeout-ms",
            "8000",
        ])
        .output()
        .expect("loop run without optout");
    assert!(
        !out.status.success(),
        "loop run must fail with a linked worktree present: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("linked worktree"),
        "FAIL must name the linked worktree: {err}"
    );
    assert!(
        err.contains("--allow-worktree"),
        "FAIL must point at the operator opt-out: {err}"
    );

    let out = Command::new(flywheel())
        .args([
            "loop-mode",
            "run",
            &project,
            "--once",
            "--timeout-ms",
            "8000",
            "--allow-worktree",
        ])
        .output()
        .expect("loop run with optout");
    assert!(
        out.status.success(),
        "loop run with --allow-worktree must succeed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("operator opt-out --allow-worktree"),
        "opt-out must be operator-logged: {all}"
    );
    assert!(
        all.contains("--once pass done"),
        "sync loop must complete its single pass: {all}"
    );

    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", sibling.to_str().unwrap()])
        .current_dir(&primary)
        .output();
    if let Some(id) = ws {
        let _ = Command::new("hc")
            .args(["workspace", "close", &id])
            .output();
    }
    let _ = std::fs::remove_dir_all(&base);
}

fn run(cwd: &Path, bin: &str, args: &[&str]) -> Output {
    let sandbox = flywheel_test_sandbox::TestSandbox::new();
    sandbox
        .command(bin)
        .args(args)
        .current_dir(cwd)
        .env("PATH", sandbox.path_env())
        .output()
        .expect("cmd runs")
}

fn run_ok(cwd: &Path, bin: &str, args: &[&str], what: &str) {
    let out = run(cwd, bin, args);
    assert!(
        out.status.success(),
        "{what} failed ({}): {}",
        bin,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Create an hc workspace with the given label + cwd; returns its id.
fn plane_workspace_create(label: &str, cwd: &str) -> Option<String> {
    let out = Command::new("hc")
        .args([
            "workspace",
            "create",
            "--label",
            label,
            "--cwd",
            cwd,
            "--no-focus",
        ])
        .output()
        .expect("hc workspace create");
    if !out.status.success() {
        eprintln!(
            "hc workspace create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v["result"]["workspace"]["workspace_id"]
                .as_str()
                .map(str::to_string)
        })
}

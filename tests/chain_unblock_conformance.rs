//! bd-2pcq Tier B #5 — CHAIN-UNBLOCK: closing a bead unblocks its
//! dependents (br dependency resolution). This is the pure-logic
//! precondition for the loop's LOOP-CHAIN completion guard (bd-bvwf.3): a
//! chain of dependent beads must not stall the batch when the blocking bead
//! closes mid-run. Proves it against the REAL `br` binary in a scratch
//! project (a hermetic .beads dir), not a mock.
//!
//! governed-by: ADR-0005 (beads tracker), bd-2pcq

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// A scratch project dir + `.beads` — hermetic, no repo state touched.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("fw-chain-{tag}-{ts}"))
}

fn br(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("br")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("br runs")
}

fn br_ok(dir: &std::path::Path, args: &[&str]) {
    let out = br(dir, args);
    assert!(
        out.status.success(),
        "br {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn br_stdout(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let out = br(dir, args);
    assert!(
        out.status.success(),
        "br {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(raw.trim())
        .unwrap_or_else(|_| serde_json::Value::String(raw.trim().to_string()))
}

fn ready_ids(v: &serde_json::Value) -> Vec<String> {
    // `br ready --json` returns a bare array in scratch projects (no
    // `{"issues": [...]}` wrapper) — accept both shapes.
    let arr = v
        .get("issues")
        .and_then(|i| i.as_array())
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default();
    arr.iter()
        .filter_map(|i| i["id"].as_str().map(str::to_string))
        .collect()
}

/// CHAIN-UNBLOCK: a bead that depends on a BLOCKED/OPEN bead is not ready;
/// closing the blocker makes the dependent READY — the precondition for the
/// loop driving a dependency chain in one run (bd-bvwf.3).
#[test]
fn closing_blocker_unblocks_dependent() {
    let dir = scratch_dir("close-unblocks");
    std::fs::create_dir_all(&dir).unwrap();
    br_ok(&dir, &["init", "--json"]);

    // b1 is the blocker; b2 depends on b1. Both carry a TYPED verify field —
    // schema-18 br's ready predicate refuses verify-less beads (bd-3566,
    // beads_rust-wave5-flywheel-parser-strip-dkpvi).
    // Fail-closed create (bd-h8d6): title alone refuses — a description is
    // required. Same titles, priority, and verify as before.
    let b1 = br_stdout(
        &dir,
        &[
            "create",
            "blocker task",
            "-p",
            "3",
            "-d",
            "conformance fixture: blocks the dependent bead",
            "--verify",
            "test -f README.md",
            "--json",
        ],
    );
    let b2 = br_stdout(
        &dir,
        &[
            "create",
            "dependent task",
            "-p",
            "3",
            "-d",
            "conformance fixture: unblocked when the blocker closes",
            "--verify",
            "test -f README.md",
            "--json",
        ],
    );
    let id1 = b1["id"].as_str().expect("b1 id").to_string();
    let id2 = b2["id"].as_str().expect("b2 id").to_string();
    br_ok(&dir, &["dep", "add", &id2, &id1]);

    // With b1 open, b2 must NOT be ready (its dependency blocks it).
    let ready_before = ready_ids(&br_stdout(&dir, &["ready", "--json"]));
    assert!(
        !ready_before.contains(&id2),
        "dependent must NOT be ready while the blocker is open: {ready_before:?}"
    );

    // Close the blocker. schema-18 fail-closed br demands a commit-sha (no
    // git repo here to cite) — this test is about dependency resolution, not
    // close legality, so manufacture it via operator bypass
    // (bd-adr0006-cosmetic-sweep-q0g0).
    let out = Command::new("br")
        .args([
            "close",
            &id1,
            "--reason",
            "done",
            "--bypass-policy",
            "--bypass-reason",
            "conformance fixture: dependency-unblock precondition, not close legality",
        ])
        .current_dir(&dir)
        .env("BR_OPERATOR", "1")
        .output()
        .expect("br runs");
    assert!(
        out.status.success(),
        "br close failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Now b2 IS ready — the chain unblocked.
    let ready_after = ready_ids(&br_stdout(&dir, &["ready", "--json"]));
    assert!(
        ready_after.contains(&id2),
        "dependent must become READY after the blocker closes: {ready_after:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

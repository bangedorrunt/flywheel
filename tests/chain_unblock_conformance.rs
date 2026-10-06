//! bd-2pcq Tier B #5 — CHAIN-UNBLOCK: closing a bead unblocks its
//! dependents (br dependency resolution). This is the pure-logic
//! precondition for the loop's LOOP-CHAIN completion guard (bd-bvwf.3): a
//! chain of dependent beads must not stall the batch when the blocking bead
//! closes mid-run. Proves it against the REAL `br` binary in a scratch
//! project (a hermetic .beads dir), not a mock.
//!
//!
//! `br create --json` has TWO shapes: a bare `{"id": …}`, and — when the id
//! is long enough to push a standard dispatch brief past br's safety margin —
//! a wrapped `{"created": {"id": …}, … "budget_warning": …}`. The old
//! reading of `["id"]` panicked on the wrapped form (bd-ly3z), and the wrap
//! was reached only because the id prefix is derived from the scratch dir's
//! name: a test failing on the operator's $TMPDIR is ambient state, not a
//! regression. Both shapes are covered now, and the wrapped one is forced by
//! a pinned long prefix rather than met by accident.
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

/// The created bead's id, whichever shape `br create --json` used: a bare
/// `{"id": …}` or the budget-wrapped `{"created": {"id": …}, "warnings":
/// [...], "budget_warning": …}`. Fails loud WITH the raw document — a panic
/// that hides the shape it choked on costs the next reader the diagnosis.
fn created_id(v: &serde_json::Value) -> String {
    v.get("id")
        .or_else(|| v.get("created").and_then(|c| c.get("id")))
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| panic!("br create --json carried no id: {v}"))
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
    // Pin a short id prefix: the id is derived from the project path, so a
    // long $TMPDIR + scratch name would silently flip the create output to
    // the wrapped shape. This test is about dependency resolution; the
    // wrapped shape has its own fixture below.
    br_ok(&dir, &["config", "set", "id.prefix", "cf"]);

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
    let id1 = created_id(&b1);
    let id2 = created_id(&b2);
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

/// bd-ly3z: BOTH `br create --json` shapes are readable, and the wrapped one
/// is FORCED here rather than met by accident. The long prefix is pinned
/// explicitly, so the shape this test exercises is a fact of the fixture, not
/// a property of the operator's $TMPDIR.
#[test]
fn budget_warning_shape_is_readable() {
    // Long enough that the derived bead id pushes a standard dispatch brief
    // past br's safety margin — the case that flipped the shape in the wild.
    const LONG: &str = "conformance-fixture-long-prefix-budget-warn";

    let long = scratch_dir("long");
    std::fs::create_dir_all(&long).unwrap();
    br_ok(&long, &["init", "--json"]);
    br_ok(&long, &["config", "set", "id.prefix", LONG]);
    let wrapped = br_stdout(
        &long,
        &[
            "create",
            "long prefix task",
            "-p",
            "3",
            "-d",
            "conformance fixture: forces br's dispatch-brief budget warning",
            "--verify",
            "test -f README.md",
            "--json",
        ],
    );
    // Anti-vacuity: if br's budget moves, this fixture stops forcing the
    // shape and the test must say so instead of passing on the bare form.
    assert!(
        wrapped.get("budget_warning").is_some() && wrapped.get("created").is_some(),
        "the pinned prefix must FORCE the wrapped shape (re-pin LONG if br's budget moved): {wrapped}"
    );
    let long_id = created_id(&wrapped);
    assert!(
        long_id.starts_with(LONG),
        "the wrapped id is the created bead's own: {long_id}"
    );

    // …and the short-prefix form STILL reads as the bare object.
    let short = scratch_dir("short");
    std::fs::create_dir_all(&short).unwrap();
    br_ok(&short, &["init", "--json"]);
    br_ok(&short, &["config", "set", "id.prefix", "cf"]);
    let bare = br_stdout(
        &short,
        &[
            "create",
            "short prefix task",
            "-p",
            "3",
            "-d",
            "conformance fixture: the unwrapped create shape",
            "--verify",
            "test -f README.md",
            "--json",
        ],
    );
    assert!(
        bare.get("budget_warning").is_none() && bare.get("id").is_some(),
        "a short pinned prefix reads as the bare create shape: {bare}"
    );
    assert!(created_id(&bare).starts_with("cf-"));

    let _ = std::fs::remove_dir_all(&long);
    let _ = std::fs::remove_dir_all(&short);
}

//! E2E: loop-run VERIFY cheap band — receipt + command-verified + stays
//! closed (bd-vsca).
//!
//! ADR-0019 §7 Wave-1 checkbox 4 (bd-w1-loop-run-verify-mbbe): on the
//! loop-runnable band the LOOP is the verifier. This suite drives the real
//! close-detect tick leaf (`__loop-run-verify` — the executor the durable
//! `loop_run_verify` function dispatches via `run_leaf_cmd`) against a
//! hermetic scratch project (temp dir + `.beads` + embedded in-process toron
//! mailbox isolated via `TORON_MCP_DATA_DIR`). Real binary, real br, real
//! git — no mocks.
//!
//! Scenarios:
//! - POSITIVE: P3/Normal bead with a loop-runnable VERIFY (`test -f
//!   README.md`); worker commits (message carries the id) then closes. The
//!   leaf classifies loop-runnable + executes with the timeout bound
//!   (exit 0). The verdict the armed loop records (`br gate report
//!   command-verified`, sha-bound) makes the close-check `legal` — the bead
//!   STAYS closed.
//! - NEGATIVE: VERIFY exits non-zero. The leaf writes the failed-head
//!   marker (`.flywheel/loop-run-verify-failed/<id>-<sha>` — the loop NEVER
//!   re-verifies the same broken head: a re-run reports `skipped:
//!   failed-head`), and the reopen leaf puts the bead back to open.
//!
//! The armed in-process receipt write + `report_pass` (loop-only,
//! anti-forgery) is covered by the verdict.rs unit roundtrip
//! (`loop_only_reporter_guard_blocks_workers_then_loop_roundtrips`); this
//! file proves the executor + decision side end-to-end against a real
//! tracker.
//!
//! governed-by: ADR-0019

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// Scratch project dir + isolated mailbox + isolated config (same hermetic
/// harness as e2e_verdict_reopen.rs — see there for the rationale).
struct Scratch {
    dir: PathBuf,
    mailbox: PathBuf,
    cfg: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("fw-{tag}-{}-{ts}", std::process::id()));
        let s = Scratch {
            dir: base.join("proj"),
            mailbox: base.join("mailbox"),
            cfg: base.join("cfg/flywheel"),
        };
        std::fs::create_dir_all(&s.dir).unwrap();
        std::fs::create_dir_all(&s.cfg).unwrap();
        // Embedded in-process toron (NO [mail.toron] url) + test identity.
        std::fs::write(
            s.cfg.join("config.toml"),
            "[backend]\nmultiplexer = \"herdr\"\nmail = \"toron\"\n\n[agent_mail]\nname = \"e2e-planner\"\n",
        )
        .unwrap();
        s
    }

    fn flywheel_env(&self) -> Vec<(&str, &str)> {
        vec![
            (
                "XDG_CONFIG_HOME",
                self.cfg.parent().unwrap().to_str().unwrap(),
            ),
            ("TORON_MCP_DATA_DIR", self.mailbox.to_str().unwrap()),
            ("FLYWHEEL_MAIL_NAME", "e2e-planner"),
        ]
    }

    fn flywheel(&self, args: &[&str]) -> Output {
        let mut c = Command::new(flywheel());
        c.args(args)
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default());
        for (k, v) in self.flywheel_env() {
            c.env(k, v);
        }
        c.output().expect("flywheel runs")
    }

    fn flywheel_json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.flywheel(args);
        assert!(
            out.status.success(),
            "flywheel {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
            .unwrap_or_else(|e| panic!("flywheel {} bad json: {e}", args.join(" ")))
    }

    fn register(&self, name: &str) {
        let out = self.flywheel(&[
            "mail",
            "register",
            self.dir.to_str().unwrap(),
            "--name",
            name,
        ]);
        assert!(
            out.status.success(),
            "register {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn run(cwd: &Path, bin: &str, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
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

fn run_json(cwd: &Path, bin: &str, args: &[&str], what: &str) -> serde_json::Value {
    let out = run(cwd, bin, args);
    assert!(
        out.status.success(),
        "{what} failed ({}): {}",
        bin,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_else(|e| {
        panic!(
            "{what} bad json: {e} (cmd: {} {}, stdout: {:?})",
            bin,
            args.join(" "),
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn br_json(cwd: &Path, args: &[&str]) -> serde_json::Value {
    run_json(cwd, "br", args, "br")
}

fn git(cwd: &Path, args: &[&str]) {
    run_ok(cwd, "git", args, "git");
}

fn bead_status(dir: &Path, bead_id: &str) -> String {
    br_json(dir, &["show", bead_id, "--json"])[0]["status"]
        .as_str()
        .unwrap_or("?")
        .to_string()
}

/// P3/Normal scratch bead with the given VERIFY, a commit carrying the id
/// (SHA rule), and the ADR-0019 gate policy. Registers the mail identities.
/// Returns (scratch, bead_id, sha).
fn scratch_runnable_bead(tag: &str, verify: &str) -> (Scratch, String, String) {
    let s = Scratch::new(tag);
    let dir = s.dir.clone();
    git(&dir, &["init", "-q"]);
    run_ok(&dir, "br", &["init"], "br init");
    std::fs::write(
        dir.join(".beads/policy.yaml"),
        "workflow:\n  transitions:\n    open:\n      - closed\n    closed:\n      - closed\n  gates:\n    open -> closed:\n      require_all:\n        - command-verified\n    closed -> closed:\n      require_all:\n        - command-verified\n",
    )
    .unwrap();
    let desc = format!("work\n\n## VERIFY\n```\n{verify}\n```");
    let created = br_json(
        &dir,
        &[
            "create", "--title", tag, "--slug", tag, "-p", "3", "-d", &desc, "--json",
        ],
    );
    let bead_id = created["id"].as_str().expect("bead id").to_string();
    std::fs::write(dir.join("README.md"), "# scratch\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(
        &dir,
        &[
            "-c",
            "user.email=test@test",
            "-c",
            "user.name=test",
            "commit",
            "-q",
            "-m",
            &format!("feat: {tag} ({bead_id})"),
        ],
    );
    let sha = String::from_utf8_lossy(&run(&dir, "git", &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let created_by = created["created_by"]
        .as_str()
        .unwrap_or("bangedorrunt")
        .to_string();
    s.register("e2e-planner");
    s.register(&created_by);
    (s, bead_id, sha)
}

fn close(dir: &Path, bead_id: &str) {
    run_ok(
        dir,
        "br",
        &["close", bead_id, "--reason", "worker done"],
        "br close",
    );
}

/// Record the command-verified gate row — the exact `br gate report` the
/// armed loop's `report_pass` invokes (note carries sha + receipt).
fn record_command_verified(dir: &Path, bead_id: &str, sha: &str) {
    run_ok(
        dir,
        "br",
        &[
            "gate",
            "report",
            bead_id,
            "--gate",
            "command-verified",
            "--provider",
            "flywheel-toron-loop1",
            "--status",
            "pass",
            "--note",
            &format!(
                "sha={sha} receipt={}/.flywheel/receipts/{bead_id}-{sha}.command-verified.txt",
                dir.display()
            ),
        ],
        "br gate report",
    );
}

/// POSITIVE: P3/Normal + loop-runnable VERIFY. The tick's executor
/// classifies loop-runnable, runs the VERIFY under the timeout bound
/// (exit 0); with the command-verified gate row bound to the derived sha the
/// close-check is `legal` and the bead STAYS closed (no reopen artifacts).
#[test]
fn loop_run_verify_cheap_band_executes_and_legal_close_stays_closed() {
    let (s, bead_id, sha) = scratch_runnable_bead("vsca-pos", "test -f README.md");
    let dir = s.dir.clone();

    // worker commits (SHA rule) + closes without a verdict
    close(&dir, &bead_id);
    assert_eq!(bead_status(&dir, &bead_id), "closed");

    // tick part 1: close-check -> missing (loop-runnable VERIFY, no row)
    let cc = s.flywheel_json(&["__close-check", dir.to_str().unwrap(), &bead_id]);
    assert_eq!(cc["status"], "missing", "close-check: {cc}");
    assert_eq!(cc["sha"], sha, "sha derived from the bead's commit");

    // tick part 2: loop-run VERIFY — classify + execute + timeout bound
    let lr = s.flywheel_json(&[
        "__loop-run-verify",
        dir.to_str().unwrap(),
        &bead_id,
        "--sha",
        &sha,
        "--timeout-secs",
        "30",
    ]);
    assert_eq!(lr["band"], "loop-runnable", "classify loop-runnable: {lr}");
    assert_eq!(lr["exit"], 0, "cheap VERIFY must exit 0: {lr}");
    assert_eq!(lr["verify"], "test -f README.md", "echo the VERIFY: {lr}");
    assert_eq!(lr["dir"], dir.to_str().unwrap(), "runs in the repo dir");

    // the armed loop records command-verified (this is the exact br command
    // `report_pass` runs); the receipt filename it would write is the
    // sha-bound machine receipt path
    record_command_verified(&dir, &bead_id, &sha);

    // tick part 3: close-check -> legal -> bead STAYS closed, no reopen
    // artifacts (the loop's legal branch removes grace + never reopens)
    for _ in 0..2 {
        let legal = s.flywheel_json(&["__close-check", dir.to_str().unwrap(), &bead_id]);
        assert_eq!(legal["status"], "legal", "close-check: {legal}");
        assert_eq!(bead_status(&dir, &bead_id), "closed", "must stay closed");
    }
    assert!(
        !dir.join(".flywheel/reopened").exists(),
        "no reopen artifacts on the verified path"
    );

    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

/// NEGATIVE: VERIFY exits non-zero. The executor writes the failed-head
/// marker (the loop NEVER re-verifies the same broken head — a re-run
/// reports `skipped: failed-head`), and the reopen leaf returns the bead to
/// open.
#[test]
fn loop_run_verify_failure_marks_failed_head_and_reopens() {
    let (s, bead_id, sha) = scratch_runnable_bead("vsca-neg", "test -f NO_SUCH_FILE_9cf0");
    let dir = s.dir.clone();
    close(&dir, &bead_id);

    // tick: close-check -> missing, then loop-run VERIFY -> non-zero exit
    let cc = s.flywheel_json(&["__close-check", dir.to_str().unwrap(), &bead_id]);
    assert_eq!(cc["status"], "missing", "close-check: {cc}");
    let lr = s.flywheel_json(&[
        "__loop-run-verify",
        dir.to_str().unwrap(),
        &bead_id,
        "--sha",
        &sha,
    ]);
    assert_eq!(lr["band"], "loop-runnable", "classify loop-runnable: {lr}");
    assert_eq!(lr["exit"], 1, "failing VERIFY must exit non-zero: {lr}");

    // failed-head marker: never re-verify the same broken head
    let marker = dir
        .join(".flywheel/loop-run-verify-failed")
        .join(format!("{bead_id}-{sha}"));
    assert!(
        marker.exists(),
        "failed-head marker missing: {}",
        marker.display()
    );

    // a re-run must report skipped, not re-execute
    let again = s.flywheel_json(&[
        "__loop-run-verify",
        dir.to_str().unwrap(),
        &bead_id,
        "--sha",
        &sha,
    ]);
    assert_eq!(
        again["skipped"], "failed-head",
        "never re-verify broken head: {again}"
    );

    // the reopen side (what the loop's tick does after a failed verify):
    // bead back to open, one mail keyed by the sha
    let r = s.flywheel_json(&["__reopen-bead", dir.to_str().unwrap(), &bead_id, &sha]);
    assert_eq!(r["ok"], true, "reopen: {r}");
    assert_eq!(r["reopened"], true, "reopen: {r}");
    assert_eq!(
        bead_status(&dir, &bead_id),
        "open",
        "failed verify must reopen"
    );

    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

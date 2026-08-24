//! E2E: unverified close reopens — live red-then-green (bd-nee7).
//!
//! ADR-0019 §7 Wave-1 checkbox 3 (bd-w1-reopen-f6tw) is unit-proven (reopen
//! 4/4) but was never exercised against the REAL loop close-detect code
//! paths + real `br` + real `git`. This suite drives the actual leaf
//! commands the durable loop's close-detect tick invokes (`__close-check`,
//! `__reopen-bead` — durable.rs `close_check` / `reopen_bead` delegate to
//! exactly these leaves via `run_leaf_cmd`) against a hermetic scratch
//! project (temp dir + `.beads` init + embedded in-process toron mailbox
//! isolated via `TORON_MCP_DATA_DIR`). No herdr, no swarm — but no mocks:
//! real binary, real br, real git, real mailbox.
//!
//! Scenarios (the loop's tick is: close-check → reopen-or-keep):
//! - RED no-commit: worker closes without a gate row AND without a commit
//!   carrying the bead id → `__close-check` says `no-commit` →
//!   `__reopen-bead` reopens (status back to open), exactly ONE mail per
//!   (bead, sha) (dedup marker survives a second leaf call).
//! - RED with-commit: commit carries the id but no gate row → `missing`
//!   (loop-runnable VERIFY is not WorkerReceipt-eligible) → reopen + one
//!   mail keyed by the derived sha.
//! - GREEN legal: commit + gate row bound to the derived sha → `legal`,
//!   bead STAYS closed, no reopen artifacts (the loop's "legal" branch
//!   removes grace + never calls the reopen leaf).
//!
//! The armed in-process receipt+report side of the tick (the loop's own
//! `report_pass` / `write_command_verified_receipt`) is covered by the
//! verdict.rs unit roundtrip (`loop_only_reporter_guard_blocks_workers_then_loop_roundtrips`);
//! this file proves the decision + reopen side end-to-end against a real
//! tracker.
//!
//! governed-by: ADR-0019

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

// ---- hermetic scratch harness -------------------------------------------------

/// Scratch project dir + isolated mailbox + isolated config. Absolute path
/// (the close-detect leaves take the project dir as the project key; the
/// repo's tmp-hygiene gate requires absolute scratch paths).
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
        // Minimal config: embedded in-process toron (NO [mail.toron] url →
        // `Mail::from_config` picks the in-process mailbox), identity
        // e2e-planner. XDG_CONFIG_HOME points here; the real machine config
        // (remote daemon at :18080) is never touched.
        std::fs::write(
            s.cfg.join("config.toml"),
            "[backend]\nmultiplexer = \"herdr\"\nmail = \"toron\"\n\n[agent_mail]\nname = \"e2e-planner\"\n",
        )
        .unwrap();
        s
    }

    /// Env for every flywheel invocation: isolated config + isolated mailbox.
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

    /// Register `name` in the scratch project's mailbox (the send/inbox
    /// paths fail fast on unknown recipients, so the reopen mail needs both
    /// the sender and the planner registered).
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

/// One bead in a scratch git+beads project. `with_commit` -> a commit whose
/// message carries the bead id (SHA rule). `with_policy` -> the ADR-0019
/// gate policy (required for `br gate report` to accept the row). Returns
/// (scratch, bead_id, sha|"").
fn scratch_bead(
    tag: &str,
    verify: &str,
    with_commit: bool,
    with_policy: bool,
) -> (Scratch, String, String) {
    let s = Scratch::new(tag);
    let dir = s.dir.clone();
    git(&dir, &["init", "-q"]);
    run_ok(&dir, "br", &["init"], "br init");
    if with_policy {
        std::fs::write(
            dir.join(".beads/policy.yaml"),
            "workflow:\n  transitions:\n    open:\n      - closed\n    closed:\n      - closed\n  gates:\n    open -> closed:\n      require_all:\n        - command-verified\n    closed -> closed:\n      require_all:\n        - command-verified\n",
        )
        .unwrap();
    }
    // bd-3566 (beads_rust-wave5-flywheel-parser-strip-dkpvi): set the TYPED
    // verify field — description fences no longer make a bead ready under
    // the schema-v18 predicate.
    let created = br_json(
        &dir,
        &[
            "create", "--title", tag, "--slug", tag, "-p", "3", "-d", "work", "--verify", verify,
            "--json",
        ],
    );
    let bead_id = created["id"].as_str().expect("bead id").to_string();
    let sha = if with_commit {
        std::fs::write(dir.join("work.txt"), "work\n").unwrap();
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
        String::from_utf8_lossy(&run(&dir, "git", &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string()
    } else {
        String::new()
    };
    // The reopen mail goes to the bead's planner (`created_by`); the sender
    // is the config identity. Register both so the real send path succeeds.
    let created_by = created["created_by"]
        .as_str()
        .unwrap_or("bangedorrunt")
        .to_string();
    s.register("e2e-planner");
    s.register(&created_by);
    (s, bead_id, sha)
}

/// Subject-filtered mail count on the bead's thread (the reopen mail is
/// `[{bead}] unverified close reopened`, thread = bead id).
fn reopen_mail_count(s: &Scratch, bead_id: &str, as_agent: &str) -> usize {
    let out = s.flywheel(&[
        "mail",
        "inbox",
        s.dir.to_str().unwrap(),
        "--as",
        as_agent,
        "--limit",
        "100",
    ]);
    assert!(
        out.status.success(),
        "mail inbox: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default();
    let arr = v.as_array().cloned().unwrap_or_else(|| {
        v.get("messages")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default()
    });
    let needle = format!("[{bead_id}] unverified close reopened");
    arr.iter()
        .filter(|m| {
            m["subject"].as_str().is_some_and(|subj| subj == needle)
                && m["thread_id"].as_str().is_some_and(|t| t == bead_id)
        })
        .count()
}

fn bead_status(dir: &Path, bead_id: &str) -> String {
    br_json(dir, &["show", bead_id, "--json"])[0]["status"]
        .as_str()
        .unwrap_or("?")
        .to_string()
}

fn close(dir: &Path, bead_id: &str, sha: &str) {
    run_ok(
        dir,
        "br",
        &[
            "close",
            bead_id,
            "--reason",
            "worker says done",
            "--commit-sha",
            sha,
        ],
        "br close",
    );
}

/// Schema-18 br refuses any close without a bead-carrying commit; an
/// operator bypass is the only way to manufacture that illegal state. The
/// loop-side NoCommit reopen stays the enforcement floor (ADR-0001 §5.3).
fn close_operator_bypass(dir: &Path, bead_id: &str) {
    let out = Command::new("br")
        .args([
            "close",
            bead_id,
            "--reason",
            "worker says done (no commit)",
            "--bypass-policy",
            "--bypass-reason",
            "test fixture: illegal no-commit close, loop floor must catch it",
        ])
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .env("BR_OPERATOR", "1")
        .output()
        .expect("br close bypass");
    assert!(
        out.status.success(),
        "br close bypass failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- the close-detect tick, driven through the real leaves ----------------

/// RED no-commit: worker closes with no gate row and no commit carrying the
/// id. The tick must reopen the bead and mail exactly ONCE per (bead, sha).
#[test]
fn unverified_close_without_commit_reopens_once_and_mails_once() {
    let s = Scratch::new("nee7-nocommit");
    let dir = s.dir.clone();
    git(&dir, &["init", "-q"]);
    run_ok(&dir, "br", &["init"], "br init");
    let created = br_json(
        &dir,
        &[
            "create",
            "--title",
            "red-nocommit",
            "--slug",
            "red-nocommit",
            "-p",
            "3",
            "-d",
            "work",
            "--verify",
            "test -f README.md",
            "--json",
        ],
    );
    let bead_id = created["id"].as_str().expect("bead id").to_string();
    let created_by = created["created_by"]
        .as_str()
        .unwrap_or("bangedorrunt")
        .to_string();
    s.register("e2e-planner");
    s.register(&created_by);

    // worker closes — no gate row, no commit carrying the id (the repo has
    // NO commits at all, so `git log --all --grep=<id>` finds nothing).
    // schema-18 br would refuse this close outright; the operator bypass
    // manufactures the illegal state the loop floor must catch.
    close_operator_bypass(&dir, &bead_id);
    assert_eq!(bead_status(&dir, &bead_id), "closed");

    // tick part 1: close-check -> no-commit
    let cc = s.flywheel_json(&["__close-check", dir.to_str().unwrap(), &bead_id]);
    assert_eq!(cc["status"], "no-commit", "close-check: {cc}");

    // tick part 2: reopen -> br status back to open + exactly one mail
    let r1 = s.flywheel_json(&["__reopen-bead", dir.to_str().unwrap(), &bead_id, ""]);
    assert_eq!(r1["ok"], true, "reopen 1: {r1}");
    assert_eq!(r1["reopened"], true, "reopen 1: {r1}");
    assert_eq!(
        bead_status(&dir, &bead_id),
        "open",
        "bead must be back open"
    );

    // dedup: a second tick must NOT re-mail (marker keyed "nocommit")
    let r2 = s.flywheel_json(&["__reopen-bead", dir.to_str().unwrap(), &bead_id, ""]);
    assert_eq!(r2["already_processed"], true, "reopen 2: {r2}");
    assert_eq!(r2["reopened"], false, "reopen 2: {r2}");
    let marker = dir
        .join(".flywheel/reopened")
        .join(format!("{bead_id}-nocommit.reopened"));
    assert!(
        marker.exists(),
        "dedup marker missing: {}",
        marker.display()
    );

    assert_eq!(
        reopen_mail_count(&s, &bead_id, &created_by),
        1,
        "exactly ONE mail per (bead, sha) — the dedup marker must suppress re-mailing"
    );

    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

/// RED with-commit: the id is in a commit message but no gate row exists.
/// close-check -> missing (a loop-runnable VERIFY is NOT WorkerReceipt-
/// eligible — row 1 of the legal-close table), then reopen + one mail keyed
/// by the derived sha.
#[test]
fn close_with_commit_but_no_gate_row_reopens() {
    let (s, bead_id, sha) = scratch_bead("nee7-missing", "test -f README.md", true, false);
    let dir = s.dir.clone();
    // schema-18 fail-closed br refuses a no-gate-row close outright; the
    // operator bypass manufactures the missing-verdict state whose
    // loop-side reopen floor this test verifies.
    close_operator_bypass(&dir, &bead_id);

    let cc = s.flywheel_json(&["__close-check", dir.to_str().unwrap(), &bead_id]);
    assert_eq!(cc["status"], "missing", "close-check: {cc}");
    assert_eq!(cc["sha"], sha, "close-check derives the bead's commit sha");
    assert_eq!(
        cc["receipt_eligible"], false,
        "loop-runnable VERIFY is NOT WorkerReceipt-eligible (legal-close row 1)"
    );

    let r = s.flywheel_json(&["__reopen-bead", dir.to_str().unwrap(), &bead_id, &sha]);
    assert_eq!(r["ok"], true, "reopen: {r}");
    assert_eq!(r["reopened"], true, "reopen: {r}");
    assert_eq!(bead_status(&dir, &bead_id), "open");

    // the marker is keyed by the derived sha
    assert!(
        dir.join(".flywheel/reopened")
            .join(format!("{bead_id}-{sha}.reopened"))
            .exists(),
        "sha-keyed dedup marker missing"
    );
    let created_by = br_json(&dir, &["show", &bead_id, "--json"])[0]["created_by"]
        .as_str()
        .unwrap_or("bangedorrunt")
        .to_string();
    assert_eq!(reopen_mail_count(&s, &bead_id, &created_by), 1);

    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

/// GREEN legal: commit + gate row bound to the derived sha. The tick's
/// close-check says `legal` — the bead STAYS closed, no reopen marker is
/// ever written (the loop's "legal" branch removes grace + never calls the
/// reopen leaf: claim-watch releases on the legal path).
#[test]
fn legal_close_with_commit_and_gate_row_stays_closed() {
    let (s, bead_id, sha) = scratch_bead("nee7-legal", "test -f README.md", true, true);
    let dir = s.dir.clone();

    // worker closes first — the loop records the verdict AFTER the close
    // (the close-detect tick runs on a closed bead). `br gate report` is
    // exactly what the armed `report_pass` invokes; the note carries
    // sha + receipt. The policy's `closed -> closed` gated self-transition
    // is what makes post-close recording legal.
    close(&dir, &bead_id, &sha);
    run_ok(
        &dir,
        "br",
        &[
            "gate",
            "report",
            &bead_id,
            "--gate",
            "command-verified",
            "--provider",
            "flywheel-toron-loop1",
            "--status",
            "pass",
            "--note",
            &format!("sha={sha} receipt={}/x.txt", dir.display()),
        ],
        "br gate report",
    );

    // two consecutive ticks: both legal, bead stays closed, zero reopen
    // artifacts (the claim-watch legal path releases without reopening)
    for _ in 0..2 {
        let cc = s.flywheel_json(&["__close-check", dir.to_str().unwrap(), &bead_id]);
        assert_eq!(cc["status"], "legal", "close-check: {cc}");
        assert_eq!(cc["sha"], sha);
        assert_eq!(
            bead_status(&dir, &bead_id),
            "closed",
            "legal close must stay closed"
        );
    }
    assert!(
        !dir.join(".flywheel/reopened").exists(),
        "no reopen artifacts on the legal path"
    );

    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}

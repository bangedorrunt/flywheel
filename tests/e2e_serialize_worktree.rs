//! E2E: serialize-on-overlap + linked-worktree preflight (bd-zord).
//!
//! ADR-0019 §7 Wave-1 checkboxes 8-10, driven against the REAL binary + real
//! `br` + real `git` + a hermetic embedded in-process toron mailbox
//! (isolated via `TORON_MCP_DATA_DIR`, same pattern as e2e_verdict_reopen):
//!
//! - Scenario A (Spec 3.4/3.5): the loop's serialize-on-overlap DATA path —
//!   reserve an always-serialize path (`.beads/**` / `Cargo.lock`) with
//!   reason = a bead id → the `__reservation-board` leaf the durable loop
//!   polls reports the row; the always-serialize gate decision derived from
//!   the REAL leaf output is TRUE (never parallel). The pure decision
//!   (`always_serialize_held` / `bead_overlap_waits` / claim-watch overlap
//!   no-nudge) lives in durable.rs and is pinned here as a source contract
//!   (event_poll_parity-style) so deleting/renaming the acceptance tests
//!   fails this suite.
//! - Scenario B (Spec 3.3): `flywheel loop run <scratch>` in a workspace
//!   carrying an extra `git worktree add` sibling exits non-zero WITHOUT
//!   `--allow-worktree` (preflight worktree check FAIL) and succeeds WITH it
//!   (operator-only, logged). Live-gated: runs when herdr + resonate are up,
//!   skips (like collab_patterns_e2e) when they are not.
//! - Assert the loop dispatch surface contains ZERO worktree-create calls
//!   (source grep) — the brief bans worktrees, the action enum has no
//!   worktree variant, and the preflight gate is the structural ban.
//!
//! governed-by: ADR-0019

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

fn herdr_up() -> bool {
    Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resonate reachable? The `--allow-worktree` success half of Scenario B
/// requires ALL preflight checks to pass, including the resonate check.
fn resonate_up() -> bool {
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
                        .any(|c| c["name"] == "resonate" && c["status"] == "pass")
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// The always-serialize path set, mirrored from `durable::ALWAYS_SERIALIZE_PATHS`
/// (ADR-0019 Spec 3.5). Kept in lockstep — the source-contract test below
/// greps durable.rs and fails if the real set drifts.
const ALWAYS_SERIALIZE_PATHS: &[&str] = &[
    ".beads/**",
    ".flywheel/**",
    "Cargo.lock",
    "Cargo.toml",
    "docs/catalog/**",
    "target/**",
];

/// Minimal overlap check: does `path` fall under any always-serialize glob?
/// Mirrors `toron_core::mailbox::overlaps` semantics for the e2e's own gate
/// decision (the loop's real decision is durable.rs `always_serialize_held`).
fn overlaps_always_serialize(path: &str) -> bool {
    ALWAYS_SERIALIZE_PATHS.iter().any(|a| {
        // glob-lite: `**` suffix matches any subpath; otherwise prefix match
        if let Some(stripped) = a.strip_suffix("/**") {
            path == stripped || path.starts_with(&format!("{stripped}/"))
        } else {
            path == *a
        }
    })
}

// ---- hermetic scratch harness (mirrors e2e_verdict_reopen) ----------------

/// Scratch project dir + isolated mailbox + isolated config. Absolute path
/// (the reservation board leaf takes the project dir as the project key; the
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
        let base = std::env::temp_dir().join(format!("fw-zord-{tag}-{}-{ts}", std::process::id()));
        let s = Scratch {
            dir: base.join("proj"),
            mailbox: base.join("mailbox"),
            cfg: base.join("cfg/flywheel"),
        };
        std::fs::create_dir_all(&s.dir).unwrap();
        std::fs::create_dir_all(&s.cfg).unwrap();
        // Minimal config: embedded in-process toron (NO [mail.toron] url →
        // `Mail::from_config` picks the in-process mailbox), identity
        // e2e-zord. XDG_CONFIG_HOME points here; the real machine config is
        // never touched.
        std::fs::write(
            s.cfg.join("config.toml"),
            "[backend]\nmultiplexer = \"herdr\"\nmail = \"toron\"\n\n[agent_mail]\nname = \"e2e-zord\"\n",
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
            ("FLYWHEEL_MAIL_NAME", "e2e-zord"),
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

    fn flywheel_ok(&self, args: &[&str]) -> Output {
        let out = self.flywheel(args);
        assert!(
            out.status.success(),
            "flywheel {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn flywheel_json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.flywheel_ok(args);
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
            .unwrap_or_else(|e| panic!("flywheel {} bad json: {e}", args.join(" ")))
    }

    /// Register `name` in the scratch project's mailbox.
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

    /// Reserve `path` as `name` with `reason` (a bead id). Returns the board
    /// JSON so the caller can inspect the row.
    fn reserve(&self, name: &str, path: &str, reason: &str) -> serde_json::Value {
        let out = self.flywheel_ok(&[
            "mail",
            "locks",
            "reserve",
            self.dir.to_str().unwrap(),
            "--paths",
            path,
            "--reason",
            reason,
            "--as",
            name,
        ]);
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
            .unwrap_or_else(|e| panic!("reserve bad json: {e}"))
    }

    /// The loop's serialize gate data: `__reservation-board` leaf output.
    fn reservation_board(&self) -> serde_json::Value {
        self.flywheel_json(&["__reservation-board", self.dir.to_str().unwrap()])
    }

    /// Release this identity's reservations.
    fn unlock(&self, name: &str) {
        let out = self.flywheel(&["mail", "unlock", self.dir.to_str().unwrap(), "--as", name]);
        assert!(
            out.status.success(),
            "unlock {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ---- Scenario A: serialize-on-overlap DATA path (hermetic) ----------------

/// The loop's serialize-on-overlap data: reserving an always-serialize path
/// (`.beads/**`, the bead tracker itself) with reason = a bead id makes the
/// `__reservation-board` leaf — the exact leaf the durable loop polls every
/// iteration (durable.rs `reservation_board`) — report the row. The gate
/// decision derived from that REAL output is TRUE (never parallel).
#[test]
fn serialize_on_overlap_board_reports_always_serialize_holds() {
    let s = Scratch::new("board");
    // git + beads tracker (the real tracker the loop reads).
    run_ok(&s.dir, "git", &["init", "-q"], "git init");
    run_ok(&s.dir, "br", &["init"], "br init");
    s.register("e2e-zord");

    // Empty board first: nothing held → no serialization.
    let board = s.reservation_board();
    assert_eq!(board["count"], 0, "empty board: {board}");

    // Hold `.beads/**` (reason = an in-flight bead id).
    s.reserve("e2e-zord", ".beads/**", "bd-zord-a");
    let board = s.reservation_board();
    assert_eq!(board["count"], 1, "one hold: {board}");
    let row = &board["reservations"][0];
    assert_eq!(row["path"], ".beads/**", "row: {row}");
    assert_eq!(
        row["reason"], "bd-zord-a",
        "reason is the holding bead: {row}"
    );
    assert_eq!(row["holder"], "e2e-zord", "holder: {row}");
    assert!(
        overlaps_always_serialize(".beads/**"),
        "`.beads/**` must be an always-serialize path"
    );

    // Release → the gate clears (loop resumes parallel dispatch).
    s.unlock("e2e-zord");
    let board = s.reservation_board();
    assert_eq!(board["count"], 0, "released board: {board}");
}

/// `Cargo.lock` (the other named always-serialize path) also gates dispatch,
/// and a NORMAL source path does not — the gate must not serialize everything.
#[test]
fn cargo_lock_gates_but_normal_source_does_not() {
    let s = Scratch::new("gate");
    run_ok(&s.dir, "git", &["init", "-q"], "git init");
    run_ok(&s.dir, "br", &["init"], "br init");
    s.register("e2e-zord");

    s.reserve("e2e-zord", "Cargo.lock", "bd-zord-b");
    let board = s.reservation_board();
    let paths: Vec<String> = board["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    assert!(
        paths.iter().any(|p| overlaps_always_serialize(p)),
        "Cargo.lock gates: {paths:?}"
    );
    s.unlock("e2e-zord");

    // Normal source path: reported on the board (other beads' overlap waits
    // still see it) but NOT an always-serialize gate.
    s.reserve("e2e-zord", "src/durable.rs", "bd-zord-c");
    let board = s.reservation_board();
    assert_eq!(board["count"], 1, "row present: {board}");
    assert!(
        !overlaps_always_serialize("src/durable.rs"),
        "normal source must NOT serialize everything"
    );
    s.unlock("e2e-zord");
}

/// The pure serialize/overlap decision + claim-watch no-nudge semantics live
/// in durable.rs and run with the default `cargo test`. Pin them here
/// (event_poll_parity-style) so a rename/delete of the acceptance tests
/// fails this suite — the e2e proves the DATA path, the unit suite proves
/// the DECISION, and this test proves the suite still exists.
#[test]
fn serialize_gate_unit_suite_is_pinned() {
    let src = std::fs::read_to_string("src/durable.rs").unwrap_or_default();
    for name in [
        "always_serialize_held",
        "bead_overlap_waits",
        "WaitReason::Overlap",
        "serialize_overlap_always_serialize_paths_gate",
        "serialize_overlap_bead_overlap_waits_marks_conflicting_in_flight",
        "serialize_overlap_claim_watch_unclaimed_overlap_no_nudge",
        "serialize_overlap_claim_watch_claimed_settled_overlap_no_close_nudge",
        "serialize_overlap_loop_never_calls_worktree_create",
    ] {
        assert!(
            src.contains(name),
            "durable.rs must keep the {name} serialize/overlap acceptance surface"
        );
    }
    // ALWAYS_SERIALIZE_PATHS must still cover .beads/** + Cargo.lock
    // (the two paths Scenario A names).
    assert!(
        src.contains(".beads/**"),
        "ALWAYS_SERIALIZE_PATHS keeps .beads/**"
    );
    assert!(
        src.contains("Cargo.lock"),
        "ALWAYS_SERIALIZE_PATHS keeps Cargo.lock"
    );
}

/// The LOOP dispatch surface has ZERO worktree-create calls: the brief's
/// FORBIDDEN bans worktrees, the loop keeper (durable.rs) never calls
/// `worktree_create` (the operator's `flywheel worktree create` CLI in
/// main.rs `cmd_worktree` is the ONLY worktree-create surface, and the loop
/// never touches it), and the preflight gate is the structural ban. Source
/// grep over the loop's keeper — this is the "assert loop logs contain zero
/// worktree calls" contract at the surface that produces those logs.
#[test]
fn loop_dispatch_surface_has_zero_worktree_creates() {
    // durable.rs is the loop keeper (run_durable_loop / run_engine_loop /
    // plan_actions): it must contain NO worktree-create call of any spelling.
    // The unit test NAME `serialize_overlap_loop_never_calls_worktree_create`
    // legitimately contains `worktree_create(` — it IS the pinned acceptance
    // test; skip that line, every other occurrence is a violation.
    let durable = std::fs::read_to_string("src/durable.rs").unwrap_or_default();
    let without_test_name: String = durable
        .lines()
        .filter(|l| !l.contains("fn serialize_overlap_loop_never_calls_worktree_create"))
        // the pinned acceptance test's OWN assertions quote the tokens
        // (`assert!(!b.contains("worktree.create"), ...)`) — those literals
        // are the pin, not violations.
        .filter(|l| !l.contains("contains(\"worktree"))
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "worktree_create(",
        "worktree.create",
        "\"git\", \"worktree\"",
        "git worktree",
    ] {
        assert!(
            !without_test_name.contains(needle),
            "durable.rs (loop keeper) must never call {needle}"
        );
    }
    // main.rs: the ONLY worktree-create calls live in cmd_worktree (the
    // operator's explicit `worktree create` CLI, lines ~10269+); the loop
    // command handler (cmd_loop, ~11084+) must not contain them. Pin by
    // ordering: the last worktree_create( occurrence must precede cmd_loop.
    let main = std::fs::read_to_string("src/main.rs").unwrap_or_default();
    let last_wt_create = main.match_indices("worktree_create(").map(|(i, _)| i).max();
    let loop_fn = main.find("fn cmd_loop(").expect("cmd_loop exists");
    if let Some(last) = last_wt_create {
        assert!(
            last < loop_fn,
            "worktree_create( must live only in cmd_worktree, before cmd_loop"
        );
    }
    // the brief bans worktrees explicitly (ADR-0019 Spec 3.2 FORBIDDEN)
    assert!(
        durable.contains("No worktree"),
        "brief FORBIDDEN bans worktrees"
    );
    // the preflight gate is the structural ban with an operator-only opt-out
    let preflight = std::fs::read_to_string("src/preflight.rs").unwrap_or_default();
    assert!(
        preflight.contains("linked_worktrees"),
        "linked-worktree filter"
    );
    assert!(
        preflight.contains("worktree_optout_legal"),
        "operator opt-out gate"
    );
}

/// The operator opt-out surface is wired: `loop run` + `workflow run
/// --execute` expose `--allow-worktree` (ADR-0019 Spec 3.3, operator-only).
#[test]
fn allow_worktree_flag_is_exposed_on_run_surfaces() {
    for sub in [
        vec!["loop", "run", "--help"],
        vec!["workflow", "run", "--execute", "--help"],
    ] {
        let out = Command::new(flywheel())
            .args(&sub)
            .output()
            .expect("help runs");
        assert!(out.status.success(), "{} --help", sub.join(" "));
        let help = String::from_utf8_lossy(&out.stdout);
        assert!(
            help.contains("--allow-worktree"),
            "{} --help must expose --allow-worktree",
            sub.join(" ")
        );
    }
}

// ---- Scenario B: linked-worktree preflight (live, herdr + resonate) --------

/// `flywheel loop run <scratch>` in a workspace with an extra `git worktree
/// add` sibling exits NON-ZERO without `--allow-worktree` (preflight FAIL
/// naming the linked tree) and SUCCEEDS with it (operator opt-out LOGGED).
/// Live-gated: needs herdr (workspace + worktree list) and resonate (all
/// preflight checks must pass for the success half). Skips when down.
#[test]
fn linked_worktree_gates_loop_run_until_operator_optout() {
    if !herdr_up() {
        eprintln!("herdr down — Scenario B skipped (CLI surface covered above)");
        return;
    }
    if !resonate_up() {
        eprintln!("resonate down — Scenario B skipped (CLI surface covered above)");
        return;
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Unique repo dir PER RUN: the herdr workspace label derives from the
    // repo dir's basename (`flywheel-<base>`), so a fixed name like
    // `primary` would collide across runs — a stale duplicate label makes
    // find_workspace resolve the WRONG (already-removed) workspace and the
    // worktree check fails with `not_git_worktree`. Run-unique base keeps
    // labels disjoint.
    let base = std::env::temp_dir().join(format!("fw-zord-live-{}-{ts}", std::process::id()));
    let primary = base.clone(); // the git repo IS the unique base dir
    std::fs::create_dir_all(&primary).unwrap();

    // scratch git repo + an extra linked worktree sibling
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

    // a herdr workspace whose label resolves the project (flywheel-<base>)
    let project = primary.to_str().unwrap().to_string();
    let label = format!(
        "flywheel-{}",
        primary.file_name().unwrap().to_str().unwrap()
    );
    let ws = herdr_workspace_create(&label, &project);

    // Scenario B part 1: WITHOUT --allow-worktree → non-zero + linked-tree FAIL
    let out = Command::new(flywheel())
        .args(["loop", "run", &project, "--once", "--timeout-ms", "8000"])
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

    // Scenario B part 2: WITH --allow-worktree → exit 0 + operator opt-out LOGGED
    let out = Command::new(flywheel())
        .args([
            "loop",
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

    // cleanup: linked sibling + scratch workspace
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", sibling.to_str().unwrap()])
        .current_dir(&primary)
        .output();
    if let Some(id) = ws {
        let _ = Command::new("herdr")
            .args(["workspace", "close", &id])
            .output();
    }
    let _ = std::fs::remove_dir_all(&base);
}

// ---- helpers ----------------------------------------------------------------

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

/// Create a herdr workspace with the given label + cwd; returns its id.
fn herdr_workspace_create(label: &str, cwd: &str) -> Option<String> {
    let out = Command::new("herdr")
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
        .expect("herdr workspace create");
    if !out.status.success() {
        eprintln!(
            "herdr workspace create failed: {}",
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

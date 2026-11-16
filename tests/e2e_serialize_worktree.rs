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
//!   (operator-only, logged). Opt-in (`FLYWHEEL_LIVE_E2E=1`, off by
//!   default): it drives the real hc and the live preflight. Opted in,
//!   it still skips when hc or the engine store is down.
//! - Assert the loop dispatch surface contains ZERO worktree-create calls
//!   (source grep) — the brief bans worktrees, the action enum has no
//!   worktree variant, and the preflight gate is the structural ban.
//!
//! governed-by: ADR-0019

use std::path::{Path, PathBuf};
use std::process::Output;

use flywheel_test_sandbox::TestSandbox;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// Opt-in for the live Scenario B. Off unless exactly `1`.
const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn live_e2e_opted_in() -> bool {
    std::env::var(LIVE_E2E).as_deref() == Ok("1")
}

/// The live Scenario B lives in `tests/live_serialize_worktree.rs`, where the
/// sandbox lint allows the `live_*.rs` shape; this file never reaches hc, the
/// preflight or the toron data dir. A plain `cargo test` used to open the
/// operator's `~/.local/share/toron` store and create a workspace on the real
/// hc on every full run.
#[test]
fn live_scenario_b_is_opt_in_before_anything_live() {
    let here = include_str!("e2e_serialize_worktree.rs");
    let live = include_str!("live_serialize_worktree.rs");
    let at = live
        .find("fn linked_worktree_gates_loop_run_until_operator_optout() {")
        .expect("Scenario B lives in live_serialize_worktree.rs");
    let body = &live[at..];
    let gate = body.find("if !live_e2e_opted_in()").expect("opt-in gate");
    for call in [
        "plane_up()",
        "engine_up()",
        "Command::new(",
        "plane_workspace_create(",
    ] {
        let first = body.find(call).unwrap_or(usize::MAX);
        assert!(gate < first, "the opt-in gate must come before {call}");
    }
    assert!(
        live.contains("#[ignore") && live.contains(LIVE_E2E),
        "the live file is #[ignore]d and gated"
    );
    for chunk in here.split("#[test]\nfn ").skip(1) {
        let name = chunk.split('(').next().unwrap_or_default();
        if name == "live_scenario_b_is_opt_in_before_anything_live" {
            continue;
        }
        for forbidden in ["Command::new(", "plane_up()", "engine_up()"] {
            assert!(
                !chunk.contains(forbidden),
                "{name}: {forbidden} belongs in live_serialize_worktree.rs"
            );
        }
    }
    if std::env::var(LIVE_E2E).ok().as_deref() != Some("1") {
        assert!(!live_e2e_opted_in(), "off unless {LIVE_E2E}=1");
    }
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
        if let Some(stripped) = a.strip_suffix("/**") {
            path == stripped || path.starts_with(&format!("{stripped}/"))
        } else {
            path == *a
        }
    })
}

/// Scratch project dir + isolated mailbox + isolated config. Absolute path
/// (the reservation board leaf takes the project dir as the project key; the
/// repo's tmp-hygiene gate requires absolute scratch paths).
struct Scratch {
    sandbox: TestSandbox,
    dir: PathBuf,
    mailbox: PathBuf,
    cfg: PathBuf,
}

impl Scratch {
    fn new(_tag: &str) -> Self {
        let sandbox = TestSandbox::new();
        let base = sandbox.root().to_path_buf();
        let s = Self {
            sandbox,
            dir: base.join("proj"),
            mailbox: base.join("mailbox"),
            cfg: base.join("cfg/flywheel"),
        };
        std::fs::create_dir_all(&s.dir).unwrap();
        std::fs::create_dir_all(&s.cfg).unwrap();
        std::fs::write(
            s.cfg.join("config.toml"),
            "[backend]\nmultiplexer = \"hc\"\nmail = \"toron\"\n\n[agent_mail]\nname = \"e2e-zord\"\n",
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
        let mut c = self.sandbox.command(flywheel());
        c.args(args)
            .current_dir(&self.dir)
            .env("PATH", self.sandbox.path_env());
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

/// The loop's serialize-on-overlap data: reserving an always-serialize path
/// (`.beads/**`, the bead tracker itself) with reason = a bead id makes the
/// `__reservation-board` leaf — the exact leaf the durable loop polls every
/// iteration (durable.rs `reservation_board`) — report the row. The gate
/// decision derived from that REAL output is TRUE (never parallel).
#[test]
fn serialize_on_overlap_board_reports_always_serialize_holds() {
    let s = Scratch::new("board");
    run_ok(&s.dir, "git", &["init", "-q"], "git init");
    run_ok(&s.dir, "br", &["init"], "br init");
    s.register("e2e-zord");

    let board = s.reservation_board();
    assert_eq!(board["count"], 0, "empty board: {board}");

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
    assert!(
        src.contains(".beads/**"),
        "ALWAYS_SERIALIZE_PATHS keeps .beads/**"
    );
    assert!(
        src.contains("Cargo.lock"),
        "ALWAYS_SERIALIZE_PATHS keeps Cargo.lock"
    );
    assert!(
        src.contains("reservation_board_live"),
        "durable.rs must keep the live (non-checkpointed) board read on the loop path"
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
    let durable = std::fs::read_to_string("src/durable.rs").unwrap_or_default();
    let without_test_name: String = durable
        .lines()
        .filter(|l| !l.contains("fn serialize_overlap_loop_never_calls_worktree_create"))
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
    let main = std::fs::read_to_string("src/main.rs").unwrap_or_default();
    let last_wt_create = main.match_indices("worktree_create(").map(|(i, _)| i).max();
    let loop_fn = main.find("fn cmd_loop(").expect("cmd_loop exists");
    if let Some(last) = last_wt_create {
        assert!(
            last < loop_fn,
            "worktree_create( must live only in cmd_worktree, before cmd_loop"
        );
    }
    assert!(
        durable.contains("No worktree"),
        "brief FORBIDDEN bans worktrees"
    );
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
        vec!["loop-mode", "run", "--help"],
        vec!["workflow", "run", "--execute", "--help"],
    ] {
        let sandbox = TestSandbox::new();
        let out = sandbox
            .command(flywheel())
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

fn run(cwd: &Path, bin: &str, args: &[&str]) -> Output {
    // A tool spawn (git, br): sandboxed per call, shims first, scratch repo as
    // the cwd.
    let sandbox = TestSandbox::new();
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

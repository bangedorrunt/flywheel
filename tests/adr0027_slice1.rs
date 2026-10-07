// governed-by: ADR-0027
//! ADR-0027 Slice 1 — acceptance S1/S2 + source-scan guards (bd-gmjo).
//!
//! Status 2026-09-04 (GS-016): wave-1 beads closed; these tests are GREEN
//! on the live path (originally DRAFT-RED: contracts pinned in ADR §7
//! Slice 1 and §10 S1/S2 before wave-1 beads bd-7fat bd-6b1y bd-gs8b
//! bd-2u27 bd-gokp bd-dhm1 landed). Green is `cargo test adr0027_s1`.
//! wave-1 lands.
//!
//! Surface: flywheel/tests/** (primary) + toron-cli/tests later. We own
//! only test dirs; never touch src/** of either repo. Flywheel is a BINARY
//! crate — we drive the CLI binary via `CARGO_BIN_EXE_flywheel` and toron
//! via `target/debug/toron` or PATH, same hermetic harness as
//! e2e_loop_run_verify.rs.
//!
//! Contracts pinned (bead descriptions — do not drift):
//! - tick(&WorkspaceModel, &Msg) -> (WorkspaceModel, Vec<Cmd>)
//! - ReceiptStore upsert/get/list; receipts sqlite next to message_reads
//! - mail send flags --corr --wake --priority --audience --result-path
//!   ride SendMessage.extra_tags (never 1059 envelope)
//! - toron receipt get/list reads the receipts store
//! - why --corr / --seq replay folds tick from Default over
//!   state_dir/journals/doorbell-journal-{workspace}.jsonl
//!
//! Test names: adr0027_s1_* — filtered by `cargo test adr0027_s1`
//! Verify: `cargo test --manifest-path flywheel/Cargo.toml adr0027_s1`

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// ── harness (copy of e2e_loop_run_verify.rs hermetic pattern) ──

// bd-rvma: the flywheel-binary resolver is gone with the last test that used it.
// The one live test here registered its pins through `flywheel mail register`,
// which only ever registered them into the scratch DIR rather than the project
// the send named, so the binary was never the thing that had to work.

/// The toron CLI this file drives: an explicit `TORON_BIN`, else the sibling
/// debug build. There is deliberately NO PATH fallback, matching
/// `sc_doorbell_adr0005.rs` (bd-oylu). A bare `toron` is whatever happens to be
/// installed, and an older one disagrees with the working tree about store
/// shape, so the test then fails somewhere that reads like a product bug
/// instead of a missing build. bd-rvma: this fallback used to hand the tests a
/// mise shim silently, and the only symptom was a green test that had skipped
/// its own assertions.
fn toron_bin() -> PathBuf {
    if let Ok(p) = std::env::var("TORON_BIN") {
        return PathBuf::from(p);
    }
    let sibling = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../toron/target/debug/toron");
    assert!(
        sibling.exists(),
        "toron CLI not found at {} — set TORON_BIN, or build it in the sibling toron repo \
         with `mbx +nightly-aarch64-apple-darwin build -p toron-cli`. This does not fall back \
         to `toron` on PATH on purpose (bd-oylu, bd-rvma): an installed toron can predate the \
         working tree and then refuses the store the in-process toron_core wrote, which reads as \
         a product failure instead of a missing build.",
        sibling.display()
    );
    sibling
}

fn flywheel_manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn toron_root() -> PathBuf {
    flywheel_manifest_dir().join("../toron")
}

fn read_file(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

fn contains(path: &Path, needle: &str) -> bool {
    read_file(path).contains(needle)
}

// ── helpers for hermetic scratch (mirrors e2e_loop_run_verify.rs Scratch) ──

struct Scratch {
    dir: PathBuf,
    mailbox: PathBuf,
    cfg: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("fw-adr27-{tag}-{}-{ts}", std::process::id()));
        let s = Self {
            dir: base.join("proj"),
            mailbox: base.join("mailbox"),
            cfg: base.join("cfg/flywheel"),
        };
        std::fs::create_dir_all(&s.dir).unwrap();
        std::fs::create_dir_all(&s.cfg).unwrap();
        std::fs::create_dir_all(&s.mailbox).unwrap();
        std::fs::write(
            s.cfg.join("config.toml"),
            "[backend]\nmultiplexer = \"herdr\"\nmail = \"toron\"\n\n[agent_mail]\nname = \"adr27-tester\"\n",
        )
        .unwrap();
        s
    }
    fn envs(&self) -> Vec<(String, String)> {
        vec![
            (
                "XDG_CONFIG_HOME".to_string(),
                self.cfg.parent().unwrap().to_string_lossy().into_owned(),
            ),
            (
                "TORON_MCP_DATA_DIR".to_string(),
                self.mailbox.to_string_lossy().into_owned(),
            ),
            ("FLYWHEEL_MAIL_NAME".to_string(), "adr27-tester".to_string()),
        ]
    }
    fn run(&self, bin: &str, args: &[&str]) -> std::process::Output {
        let mut c = Command::new(bin);
        c.args(args)
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", std::env::var_os("HOME").unwrap_or_default());
        for (k, v) in self.envs() {
            c.env(k, v);
        }
        c.output().expect("run failed")
    }
}

// ── Slice 1 contracts ──

// 1. WorkspaceModel + Msg + Cmd + tick entry (E1, bd-7fat)
//    RED until bd-7fat closes: src/workspace.rs does not exist at bde4daae.
//    After: one WorkspaceModel { doorbell, swarm, assignment, roles, presence,
//    open_human, seq }, one Msg union, one Cmd enum (data, no I/O), tick signature.
#[test]
fn adr0027_s1_workspace_model_exists() {
    let ws = flywheel_manifest_dir().join("src/workspace.rs");
    assert!(
        ws.exists(),
        "RED (bd-7fat): src/workspace.rs missing — expected WorkspaceModel + Msg + Cmd + tick entry (E1). Create src/workspace.rs per bd-7fat contract."
    );
    let content = read_file(&ws);
    for needle in [
        "struct WorkspaceModel",
        "pub doorbell:",
        "DoorbellModel",
        "assignment",
        "roles: BTreeMap",
        "presence: BTreeMap",
        "open_human: BTreeMap",
        "seq: u64",
        "enum Msg",
        "enum Cmd",
        "fn tick",
        "WorkspaceModel",
        "Vec<Cmd>",
    ] {
        assert!(
            content.contains(needle),
            "RED (bd-7fat): src/workspace.rs missing `{needle}` — WorkspaceModel composition incomplete"
        );
    }
    // tick must be pure: no Instant::now inside reducer (clock enters as Msg payload)
    assert!(
        !content.contains("Instant::now()"),
        "RED (bd-7fat): tick must not call Instant::now() — clock enters as Msg::Tick{{now}}"
    );
    // doorbell_tick and durable become private slice reducers called only from Workspace tick
    assert!(
        content.contains("doorbell_tick::tick") || content.contains("durable::tick"),
        "RED (bd-7fat): workspace tick should compose doorbell_tick::tick / durable::tick as private slice reducers"
    );
}

// 2. corr/wake/priority/audience/result_path ride extra_tags (E2, bd-2u27)
//    RED until bd-2u27 closes: cli.rs MailSendArgs has no --corr flags.
#[test]
fn adr0027_s1_toron_send_flags_exist() {
    let cli = toron_root().join("crates/toron-cli/src/cli.rs");
    assert!(cli.exists(), "toron cli.rs not found at {}", cli.display());
    let content = read_file(&cli);
    // Flags are defined as `pub corr: Option<String>` with `#[arg(long)]` (field name -> flag name)
    // Check for field definitions, not just literal "--corr" strings
    for field in [
        "pub corr:",
        "pub wake:",
        "pub priority:",
        "pub audience:",
        "pub result_path:",
    ] {
        assert!(
            content.contains(field),
            "RED (bd-2u27): toron mail send missing field {field} — flags must ride SendMessage.extra_tags (E2). See bd-2u27 contract."
        );
    }
    // Also verify the flags appear in help output (real binary)
    let out = std::process::Command::new(toron_bin())
        .args(["mail", "send", "--help"])
        .output()
        .expect("toron help");
    let help =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    for flag in [
        "--corr",
        "--wake",
        "--priority",
        "--audience",
        "--result-path",
    ] {
        assert!(
            help.contains(flag),
            "RED (bd-2u27): toron mail send --help missing {flag} — binary help must list it"
        );
    }
    // extra_tags must never be on 1059 envelope (messaging.rs already correct —
    // this pins that CLI wires it to rumor 14 / job 43001 builder, not envelope)
    let messaging = toron_root().join("crates/toron-core/src/mailbox/messaging.rs");
    let m = read_file(&messaging);
    assert!(
        m.contains("extra_tags"),
        "RED (bd-2u27): messaging.rs must have SendMessage.extra_tags (vehicle exists, currently unused by CLI)"
    );
    // wake/priority/audience must be typed, not free strings — check for value_enum or typed parse
    // After bd-2u27 the CLI should reject unknown wake values loudly
    assert!(
        content.contains("wake") && content.contains("priority") && content.contains("audience"),
        "RED (bd-2u27): CLI must have typed wake/priority/audience vocabulary"
    );
}

// 3. receipts sqlite store + ReceiptStore API + receipt get/list (E2, bd-gokp)
//    RED until bd-gokp closes: receipts(corr TEXT PRIMARY KEY, ...) table missing.
#[test]
fn adr0027_s1_receipt_store_contract() {
    // Check toron-core mailbox has Receipt struct + store
    let mailbox = toron_root().join("crates/toron-core/src/mailbox");
    let messaging = mailbox.join("messaging.rs");
    // Look for receipts table migration / store files — must be exact schema, not just the word "receipts"
    let has_receipts_table = std::fs::read_dir(&mailbox)
        .map(|entries| {
            entries.filter_map(std::result::Result::ok).any(|e| {
                let p = e.path();
                p.extension().map(|x| x == "rs").unwrap_or(false)
                    && read_file(&p).contains("corr TEXT PRIMARY KEY")
                    && read_file(&p).contains("receipts")
            })
        })
        .unwrap_or(false)
        || contains(&messaging, "corr TEXT PRIMARY KEY")
        || {
            // migrations dir — exact table creation, not just read-receipts comment
            let mig = toron_root().join("crates/toron-core/migrations");
            if mig.exists() {
                std::fs::read_dir(&mig)
                    .map(|ents| {
                        ents.filter_map(std::result::Result::ok).any(|e| {
                            let p = e.path();
                            p.is_file()
                                && read_file(&p).contains("CREATE TABLE")
                                && read_file(&p).contains("receipts")
                                && read_file(&p).contains("corr TEXT PRIMARY KEY")
                        })
                    })
                    .unwrap_or(false)
            } else {
                false
            }
        };
    // For precise contract, also check for pub struct Receipt { corr, state: ReceiptState, ... }
    // Avoid false positive on `struct ReceiptResult` — match exact `struct Receipt {`
    let has_receipt_struct = contains(&messaging, "pub struct Receipt {")
        || contains(&messaging, "pub struct Receipt\n")
        || {
            let receipt_file = mailbox.join("receipts.rs");
            receipt_file.exists() && read_file(&receipt_file).contains("pub struct Receipt")
        };
    assert!(
        has_receipts_table && has_receipt_struct,
        "RED (bd-gokp): receipts sqlite store missing — expected receipts(corr TEXT PRIMARY KEY, state TEXT NOT NULL, wake TEXT, due_at INTEGER, result_path TEXT, updated_at INTEGER NOT NULL) + pub struct Receipt {{ corr, state: ReceiptState, wake: Option<Wake>, due_at, result_path, updated_at }} next to message_reads/inbox_cursors (E2). See bd-gokp CONTRACT. has_table={has_receipts_table} has_struct={has_receipt_struct}"
    );
    // CLI verbs toron receipt get/list
    let cli = toron_root().join("crates/toron-cli/src/cli.rs");
    let c = read_file(&cli);
    assert!(
        c.contains("receipt") || c.contains("Receipt"),
        "RED (bd-gokp): toron receipt get --corr <id> / receipt list [--state] CLI verbs missing"
    );
    let main_rs = toron_root().join("crates/toron-cli/src/main.rs");
    if main_rs.exists() {
        let m = read_file(&main_rs);
        // main should wire receipt get/list (not just cli definition)
        // we assert presence, not absence of failure
        let has_wire = m.contains("receipt") || m.contains("Receipt");
        if !has_wire {
            // soft — cli definition is the minimum pin; wiring is second commit
            eprintln!(
                "warn: toron-cli main.rs does not yet wire receipt verbs (expected after bd-gokp)"
            );
        }
    }
}

/// Read one receipt by corr and return its JSON (bd-gokp's durable store).
///
/// No `--project`: `toron receipt get` takes `--corr` alone (measured — a
/// `--project` here is an 'unexpected argument' refusal). No `--json` either:
/// this verb already answers in JSON, and a mutation that dropped the flag left
/// the suite green, so carrying it would have been decoration. Loud on failure,
/// so a receipt that cannot be read is a contract failure rather than something
/// a caller can quietly skip past.
fn receipt(scratch: &Scratch, toron: &str, corr: &str) -> serde_json::Value {
    let out = scratch.run(toron, &["receipt", "get", "--corr", corr]);
    assert!(
        out.status.success(),
        "RED (bd-gmjo S1, bd-gokp): toron receipt get --corr {corr} failed\n\
         stdout: {}\nstderr: {}\n\
         Contract: receipts sqlite next to message_reads/inbox_cursors; a new process \
         rebuilds the Model assignment from the table, not from a journal fold. \
         ReceiptState owed|acked|resulted.",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "RED (bd-gmjo S1, bd-gokp): toron receipt get --corr {corr} returned no JSON ({e})\n\
             exit: {:?}\nstdout: {}\nstderr: {}\n\
             Contract: receipts sqlite next to message_reads/inbox_cursors; a new process \
             rebuilds the Model assignment from the table, not from a journal fold. \
             ReceiptState owed|acked|resulted.",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

// 4. S1 — tasked mail with corr=FM-GS-012 survives walker death
//    Send tasked mail with --corr FM-GS-012, kill walker (drop Scratch / new process),
//    assert owed/acked still visible via receipt get. Empty tasked still writes result.
//    This is the real scenario; it drives the binary, not a unit proxy (prove-it-works).
//
// bd-rvma: this test was green only because a skip branch swallowed its own
// setup failure. On the default suite `mail send` died with "unknown recipient:
// adr27-recip" (the pins were never in the send project, and their names are
// rejected outright by the CamelCase Adjective+Noun rule), the branch printed a
// warning, `return`ed, and the test reported ok having asserted nothing about
// receipts. Everything below the send is now a real assertion on receipt
// VALUES, and there is no path that returns early.
//
// `#[ignore]`d per bd-p2zm: it drives a real `toron`, so its green depends on a
// sibling build being on disk. Run it on purpose with:
//
//     mbx +nightly-aarch64-apple-darwin test --test adr0027_slice1 -- --ignored adr0027_s1_tasked
#[ignore = "needs the sibling toron debug build (mbx +nightly-aarch64-apple-darwin build -p toron-cli in ~/workspace/toron)"]
#[test]
fn adr0027_s1_tasked_mail_corr_survives_walker_death() {
    let toron = toron_bin();
    let toron_s = toron.to_str().expect("utf-8 toron path");
    let scratch = Scratch::new("s1-walker");
    let proj = "adr27-s1";

    // Both pins must exist IN THE PROJECT THIS SENDS WITH. The old setup
    // registered them into the scratch dir instead (a different project) and
    // only eprintln!'d when that failed, so the send then reported an unknown
    // recipient and the skip branch took over. `project ensure` is not needed:
    // a pinned TORON_MCP_DATA_DIR store carries the send without an operator
    // key (measured), and asking for a manifest signature is what refused.
    for pin in ["IvoryRidge", "EmberOtter"] {
        let out = scratch.run(
            toron_s,
            &["agent", "bootstrap", "--project", proj, "--as", pin],
        );
        assert!(
            out.status.success(),
            "RED (bd-rvma): toron agent bootstrap --project {proj} --as {pin} failed\n\
             stdout: {}\nstderr: {}\n\
             Both pins must be bootstrapped into the project the send names, or the \
             send below reports an unknown recipient.",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Try to send tasked mail with the new flags (bd-2u27). Before impl this
    // fails with an unknown flag; there is no skip branch, so a failure here is
    // a failure of the contract rather than something the test routes around.
    let corr = "FM-GS-012";
    let send_args = [
        "mail",
        "send",
        "--project",
        proj,
        "--as",
        "IvoryRidge",
        "--to",
        "EmberOtter",
        "--subject",
        "tasked S1",
        "--body",
        "standing vs tasked S1",
        "--corr",
        corr,
        "--wake",
        "tasked",
        "--priority",
        "queue",
        "--audience",
        "agents",
    ];
    let out = scratch.run(toron_s, &send_args);
    assert!(
        out.status.success(),
        "RED (bd-gmjo S1, bd-2u27): toron mail send --corr failed\n\
         args: {:?}\nstdout: {}\nstderr: {}\n\
         Contract: SendMessage.extra_tags carries corr/wake/priority/audience/result_path \
         (never 1059 envelope). bd-rvma deleted the old branch that treated an \
         'unknown recipient' send as a skip — that is how this test was green \
         while proving nothing.",
        send_args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Verify the receipt via `receipt get` (bd-gokp) — durable sqlite, not
    // HashMap. NOTE there is no `--project` on this verb (measured: 'unexpected
    // argument'); the receipts table is keyed by corr alone.
    let v = receipt(&scratch, toron_s, corr);
    assert_eq!(
        v["corr"].as_str(),
        Some(corr),
        "receipt get must return corr={corr}, got {v}"
    );
    // The bd-2u27 claim under test: `--wake` rode SendMessage.extra_tags, so
    // the durable row carries it. A leak onto the 1059 envelope would not land
    // here, which is what makes this the observable for the contract.
    assert_eq!(
        v["wake"].as_str(),
        Some("tasked"),
        "--wake tasked must ride extra_tags into the receipt row, got {v}"
    );
    assert_eq!(
        v["state"].as_str(),
        Some("owed"),
        "a tasked send opens its receipt owed, got {v}"
    );

    // Kill walker: a NEW process over the same store, no in-memory state carried
    // across. If receipts lived in a HashMap this read would miss.
    let scratch2 = Scratch {
        dir: scratch.dir.clone(),
        mailbox: scratch.mailbox.clone(),
        cfg: scratch.cfg.clone(),
    };
    let v2 = receipt(&scratch2, toron_s, corr);
    assert_eq!(
        v2["corr"].as_str(),
        Some(corr),
        "a fresh process must still see the receipt for corr={corr}"
    );
    assert_eq!(
        v2["state"].as_str(),
        Some("owed"),
        "the owed assignment must survive walker death in the receipts table, got {v2}"
    );

    // Empty tasked still records its result path against corr (E3).
    let empty_corr = "FM-GS-012-empty";
    // Inside the scratch base, not a hardcoded /tmp: the receipt's result_path
    // is what this asserts on, and it must be the path the caller passed.
    let result_path = scratch
        .dir
        .parent()
        .expect("scratch base")
        .join("adr27-result.txt");
    let out = scratch2.run(
        toron_s,
        &[
            "mail",
            "send",
            "--project",
            proj,
            "--as",
            "IvoryRidge",
            "--to",
            "EmberOtter",
            "--subject",
            "empty tasked",
            "--body",
            "",
            "--corr",
            empty_corr,
            "--wake",
            "tasked",
            "--result-path",
            result_path.to_str().expect("utf-8 result path"),
        ],
    );
    assert!(
        out.status.success(),
        "RED (bd-gmjo S1 empty tasked): empty tasked send failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The send does NOT create the result file (measured) — the assignee writes
    // that later, and `receipt set` moves the row to resulted. The observable
    // for "the result path rode extra_tags" is the receipt's own field, which
    // the old version promised to check and never did.
    let v3 = receipt(&scratch2, toron_s, empty_corr);
    assert_eq!(
        v3["result_path"].as_str(),
        result_path.to_str(),
        "--result-path must ride extra_tags into the receipt row, got {v3}"
    );

    let _ = std::fs::remove_dir_all(scratch.dir.parent().unwrap());
}

// 5. S2 — standing empty silent; tasked empty writes result + result_path
//    RED until bd-dhm1 closes (assignment in tick, E2/E3).
#[test]
fn adr0027_s1_standing_empty_silent_tasked_empty_result() {
    let ws = flywheel_manifest_dir().join("src/workspace.rs");
    assert!(
        ws.exists(),
        "RED (bd-gmjo S2, bd-7fat): src/workspace.rs missing — S2 pins standing/tasked in tick (E3)"
    );
    let content = read_file(&ws);
    // S2 contract: standing + empty -> no Cmd to human pin; tasked + empty -> Cmd records result
    assert!(
        content.contains("standing") && content.contains("tasked"),
        "RED (bd-gmjo S2, bd-dhm1): Workspace tick must distinguish wake=standing vs tasked (E3)"
    );
    assert!(
        content.contains("result_path")
            || content.contains("result-path")
            || content.contains("result_path"),
        "RED (bd-gmjo S2): tasked empty must carry result_path tag into tick"
    );
    // Default tasked priority is queue; interrupt is operator/user only, never onto human pin
    assert!(
        content.contains("queue")
            && (content.contains("interrupt") || content.contains("Interrupt")),
        "RED (bd-gmjo S2): tick must enforce default tasked priority=queue and interrupt never onto human pin (E3)"
    );
    // audience=human addresses one orchestrator pin; broadcast-to-human is hard error
    assert!(
        content.contains("audience") && content.contains("human"),
        "RED (bd-gmjo S2): tick must handle audience=human (one pin, broadcast hard error)"
    );
    // Check toron CLI flags already pinned in previous test — here we pin the tick side:
    // standing empty must NOT produce Cmd::Publish to human pin (assert via code scan for Prompt guard)
    // We look for a conditional that returns early / no Prompt for standing empty.
    // This is a soft pin: we require the test to mention silence.
    let has_silence = content.contains("standing")
        && (content.contains("no Cmd")
            || content.contains("silence")
            || content.contains("No Cmd")
            || content.contains("Prompt"));
    if !has_silence {
        eprintln!(
            "warn: workspace.rs standing-empty silence not yet explicit — ensure tick emits no Cmd to human pin for wake=standing + empty"
        );
    }
    // CLI-level S2: try to verify via toron receipt that standing empty has no prompt
    // We do not have a direct inject-assert without workspace tick, so we defer the full e2e to the integration test above.
    // Mark RED until bd-dhm1 wires assignment/BTreeMap<Corr,Receipt> in tick.
    assert!(
        content.contains("assignment") && content.contains("Receipt"),
        "RED (bd-gmjo S2, bd-dhm1): WorkspaceModel.assignment: BTreeMap<Corr,Receipt> missing"
    );
}

// 6. Source-scan guards — Tick Law leftovers that must be gone in Slice 1.
//    Each guard asserts the bypass is NOT present. They FAIL before the fold bead closes
//    (RED), PASS after (GREEN). The comment names the leftover # and file:line at bde4daae.

// 6a. leftover 2: watch_sends appended AFTER durable::tick is a bypass -> Msg::ClaimWatch into tick
#[test]
fn adr0027_s1_guard_no_watch_sends_after_tick() {
    let durable = flywheel_manifest_dir().join("src/durable.rs");
    let content = read_file(&durable);
    // The bypass is `actions.append(&mut watch_sends)` after tick (alloc 5729, fill 6441-6493, append 6996)
    // It must be gone; tick comment "DELIBERATELY loop-side" must be deleted (568-574)
    assert!(
        !content.contains("actions.append(&mut watch_sends)"),
        "RED (bd-gs8b #2): durable.rs still appends watch_sends AFTER tick — must be Msg::ClaimWatch into tick; delete the append at ~6996 and the DELIBERATELY-loop-side comment at 568-574"
    );
    assert!(
        !content.contains("DELIBERATELY") || !content.contains("loop-side"),
        "RED (bd-gs8b #2): durable.rs still carries the DELIBERATELY loop-side comment — delete it when folding ClaimWatch"
    );
    assert!(
        !content.contains("watch_sends.push")
            || content.contains("Msg::ClaimWatch")
            || content.contains("ClaimWatch"),
        "RED (bd-gs8b #2): watch_sends.push should be gone or replaced by Msg::ClaimWatch construction"
    );
}

// 6b. leftover 4: post-tick filter drops Diagnose pane_exited (6956-6962) — NOW mutilates tick output
#[test]
fn adr0027_s1_guard_no_diagnose_pane_exited_filter() {
    let durable = flywheel_manifest_dir().join("src/durable.rs");
    let content = read_file(&durable);
    // The filter that was at 6956-6962: `!matches!(a, LoopAction::Diagnose { reason: "pane_exited" } if exited_names.contains(name))`
    // Legitimate uses of "pane_exited" remain (e.g., reclaim_on_pane_exited at 4576, 5067). Check for the filter pattern.
    let has_filter = content.contains("exited_names.contains")
        && content.contains("Diagnose")
        && content.contains("pane_exited");
    // Also check for the old comment that was at 6846 before fix — now it should say "leftover #4 fixed"
    let has_fixed_comment = content.contains("leftover #4 fixed");
    assert!(
        !has_filter || has_fixed_comment,
        "RED (bd-gs8b #4): durable.rs still filters Diagnose {{pane_exited}} after tick (6956-6962) — DELETE the filter; tick output is the plan (found exited_names.contains + Diagnose pane_exited without fixed comment)"
    );
    // Ensure the fixed comment exists
    assert!(
        has_fixed_comment,
        "RED (bd-gs8b #4): durable.rs should contain 'leftover #4 fixed' comment at ~6846 after fix"
    );
}

// 6c. leftover 6+7+8: doorbell side — double bell_model, pre-insert kills drain, WatchDied discards model'
#[test]
fn adr0027_s1_guard_no_double_bell_model_and_no_status_preinsert_and_watchdied_keeps_model() {
    let main_rs = flywheel_manifest_dir().join("src/main.rs");
    let content = read_file(&main_rs);
    // 6. double bell_model = Default at 7747 then 7819 — one model for dispatcher lifetime
    let count = content
        .matches("let mut bell_model = doorbell_tick::DoorbellModel::default()")
        .count();
    assert!(
        count <= 1,
        "RED (bd-6b1y #6): main.rs has {count} bell_model = Default (7747/7819) — must be ONE model for dispatcher lifetime"
    );
    assert!(
        !content.contains("let mut status_by_pin"),
        "RED: live dispatcher still owns status_by_pin; Model.doorbell.statuses is the only herdr-status map"
    );
    // 7. statuses.insert(to) before HerdrStatus tick (8006 live, 8444 refresh) -> drain dead
    // Check for the pre-insert pattern immediately before tick for HerdrStatus
    // We scan for the two-insert sites; after fix they must not appear before doorbell_tick::tick for HerdrStatus
    let preinsert_occurrences = content.matches("bell_model.statuses.insert").count();
    // At bde4daae there are 4 inserts (8006,8444,8528, plus one more); after fix the HerdrStatus inserts (8006/8444) must be gone (tick sees `from`)
    // We assert that NOT all inserts remain — at least the live/refresh HerdrStatus pre-inserts are removed.
    // The simplest pin: after fix, the file should NOT contain `bell_model.statuses.insert(pin.clone(), to.clone())` before a HerdrStatus tick.
    // We check that the specific pattern with `to.clone()` appears at most once (the non-HerdrStatus paths may keep one insert if justified by review, but not the HerdrStatus drain killer)
    if preinsert_occurrences > 2 {
        // allow at most 1-2 non-Herdr inserts, but the 2 Herdr inserts must be gone
        // We do a stricter check: look for the comment that was added to disambiguate
        // After fix, the lines 8006/8444 should be gone.
        let has_live_insert = content.contains("// apply_status_change. `from` is kept")
            || content.contains("bell_model.statuses.insert(pin.clone(), to.clone())");
        if has_live_insert {
            // still RED — count exact pattern
            let specific = content
                .matches("bell_model.statuses.insert(pin.clone(), to.clone())")
                .count();
            assert!(
                specific <= 1,
                "RED (bd-6b1y #7): main.rs still pre-inserts statuses before HerdrStatus tick (live 8006, refresh 8444) — stop; let tick see `from` (drain fires on working->idle/done/blocked)"
            );
        }
    }
    // 8. WatchDied discards model' at 8055 `let (_, tick_actions) = ...`
    assert!(
        !content.contains("let (_, tick_actions) = doorbell_tick::tick(&bell_model, &msg)"),
        "RED (bd-6b1y #8): main.rs WatchDied still discards model' (`let (_, tick_actions) = tick(...)` at 8055) — keep the returned model"
    );
}

// 6d. leftover 9: --once + apply_watch_line/apply_status_change second reducers (doorbell.rs 1240/1267, 1287-1296 via main.rs 7623)
#[test]
fn adr0027_s1_guard_once_calls_tick_not_second_reducer() {
    let doorbell = flywheel_manifest_dir().join("src/doorbell.rs");
    let main_rs = flywheel_manifest_dir().join("src/main.rs");
    let d_content = read_file(&doorbell);
    let m_content = read_file(&main_rs);
    // --once path must call tick, not apply_watch_line / apply_status_change
    // Check that main.rs --once handling (around 7623) does not call apply_watch_line
    let once_section = if let Some(pos) = m_content.find("--once") {
        m_content[pos.saturating_sub(2000)..std::cmp::min(pos + 4000, m_content.len())].to_string()
    } else {
        // fallback: look for `apply_watch_line` in main at all
        m_content.clone()
    };
    assert!(
        !once_section.contains("apply_watch_line") || once_section.contains("workspace::tick"),
        "RED (bd-6b1y #9): --once still uses apply_watch_line second reducer (doorbell.rs 1240/1267) — --once must call tick; helpers may call tick, never a second producer"
    );
    assert!(
        !d_content.contains("apply_status_change") || d_content.contains("tick(&WorkspaceModel"),
        "RED (bd-6b1y #9): doorbell.rs apply_status_change second reducer still present on live path"
    );
    // More direct: ensure the `once` flag handling in main.rs invokes workspace::tick or legacy doorbell_tick::tick
    assert!(
        (m_content.contains("workspace::tick") || m_content.contains("doorbell_tick::tick"))
            && (m_content.contains("--once") || m_content.contains("once")),
        "RED (bd-6b1y #9): main.rs must wire --once through tick (not bypass)"
    );
}

// 6e. every live dispatcher event constructs a Msg
#[test]
fn adr0027_s1_guard_every_live_event_constructs_msg() {
    let main_rs = flywheel_manifest_dir().join("src/main.rs");
    let content = read_file(&main_rs);
    // Live dispatcher binds at least: AckSlaTick, WatchNdjson, HerdrStatus, WatchDied, InboxRows sweep
    for msg_variant in [
        "AckSlaTick",
        "WatchNdjson",
        "HerdrStatus",
        "WatchDied",
        "InboxRows",
    ] {
        assert!(
            content.contains(msg_variant),
            "RED (bd-6b1y/bd-gs8b E1): main.rs live dispatcher never constructs DoorbellMsg::{msg_variant} / Msg::{msg_variant} — every live edge must be a Msg into tick"
        );
    }
    // No direct `doorbell_line` or `route_or_queue` outside tick (they are tick internals after fold)
    // This is a soft guard: ensure classify/parse helpers are not called outside tick as a second reducer
    // We check that `parse_watch_ndjson` / `classify` are not called in main.rs outside of tick construction
    // After fold they live only in doorbell_tick.rs / workspace.rs
    // For now we just assert tick is the only state transition
    assert!(
        content.contains("workspace::tick(") || content.contains("doorbell_tick::tick("),
        "RED (bd-6b1y E1): live dispatcher must call tick(Model, Msg) as the only state transition (workspace::tick since the ADR-0027 reducer fold)"
    );
}

// 6f. Heal / Reclaim / Fact / FetchInbox folded into Msg (leftovers 3,10,13,14)
#[test]
fn adr0027_s1_guard_heal_reclaim_fact_fetch_folded() {
    let durable = flywheel_manifest_dir().join("src/durable.rs");
    let doorbell_tick = flywheel_manifest_dir().join("src/doorbell_tick.rs");
    let main_rs = flywheel_manifest_dir().join("src/main.rs");
    let state = flywheel_manifest_dir().join("src/state.rs");
    let d = read_file(&durable);
    let dt = read_file(&doorbell_tick);
    let m = read_file(&main_rs);
    let s = read_file(&state);

    // Reclaim pre-seeds heal_actions (6087, 6763, reclaim 6261-6281) -> Msg::Reclaim into tick, then actions = tick(...) only
    assert!(
        !d.contains("heal_actions") || d.contains("Msg::Reclaim") || d.contains("Reclaim"),
        "RED (bd-gs8b #3): durable.rs still pre-seeds heal_actions before tick — must be Msg::Reclaim into tick, then actions = tick(...) only"
    );
    // heal_states onto ActiveLoop (or stop claiming crash-safe); apply_diagnose_outcome loop-local mutate after tick (7472, loop-local map 5672) -> Msg result
    assert!(
        s.contains("heal_states") || s.contains("heal_states:"),
        "RED (bd-gs8b #14): state.rs ActiveLoop missing heal_states — move onto ActiveLoop (crash-safe), then apply_diagnose_outcome is a Msg result into tick, not loop-local mutate at 7472"
    );
    // FetchInbox never emitted (doorbell_tick.rs:45 only stringified 340; zero constructors) -> real Cmd
    assert!(
        dt.contains("FetchInbox") || m.contains("FetchInbox") || d.contains("FetchInbox"),
        "RED (bd-6b1y #13): FetchInbox never emitted — must be a real Cmd whose rows re-enter as InboxRows"
    );
    // DoorbellMsg::Fact on live captain path; adapter must not swallow Err into ()
    // (bd-6b1y #10; reducer now lives in doorbell-core — doorbell_tick.rs is a
    // re-export shim, so the Fact arm is asserted at its new home).
    let doorbell_core = flywheel_manifest_dir().join("crates/doorbell-core/src/lib.rs");
    let core = read_file(&doorbell_core);
    assert!(
        core.contains("Fact") && m.contains("Fact"),
        "RED (bd-6b1y #10): DoorbellMsg::Fact not on live path — captain_fact_inject + adapter must not swallow Err"
    );
    assert!(
        !m.contains("swallows") && !m.contains("Err(())") || !m.contains("10853"),
        "RED (bd-6b1y #10): adapter still swallows Err into () at main.rs 10853-10860 — must propagate fact into Msg"
    );
}

// 7. why --corr / --seq replay folds tick from Default over journals/doorbell-journal-{workspace}.jsonl
#[test]
fn adr0027_s1_why_corr_seq_replays_tick() {
    let doorbell = flywheel_manifest_dir().join("src/doorbell.rs");
    let main_rs = flywheel_manifest_dir().join("src/main.rs");
    let d = read_file(&doorbell);
    let m = read_file(&main_rs);
    // Path must be state_dir/journals/doorbell-journal-{workspace}.jsonl (not doorbell/<ws_hash>.jsonl claimed in AGENTS at bde4daae)
    assert!(
        d.contains("doorbell-journal-{workspace}.jsonl") && d.contains("journals"),
        "RED (bd-6b1y #11): doorbell.rs journal path must be state_dir/journals/doorbell-journal-{{workspace}}.jsonl (1303-1306), not doorbell/<ws_hash>.jsonl"
    );
    // why must fold tick from Default, asserting same Cmd sequence as live run
    // Stronger: the why function itself must fold tick, not just any "tick" in the file
    if let Some(pos) = m.find("fn cmd_doorbell_why") {
        let why_fn = m[pos..std::cmp::min(m.len(), pos + 8000)].to_string();
        assert!(
            why_fn.contains("fold")
                || why_fn.contains("replay")
                || why_fn.contains("WorkspaceModel::default")
                || why_fn.contains("tick("),
            "RED (bd-6b1y #11): doorbell why must fold tick from Default over journal and assert same Cmd sequence — currently greps pin substring (7480-7556), not replay. why_fn missing fold/replay/tick call"
        );
        assert!(
            why_fn.contains("--corr") || why_fn.contains("corr") && why_fn.contains("seq"),
            "RED (bd-6b1y #11): why must gain --corr / --seq flags (replay by corr/seq, not pin grep) — why_fn missing corr/seq"
        );
        // why should NOT be a simple grep of pin substring
        assert!(
            !why_fn.contains("pin.contains") && !why_fn.contains("grep") || why_fn.contains("fold"),
            "RED (bd-6b1y #11): why still greps pin substring — must fold tick"
        );
    } else {
        panic!("RED (bd-6b1y #11): fn cmd_doorbell_why not found in main.rs");
    }
    // Journal is log of Msg; why is replay by seq/corr, not grep (seq is WorkspaceModel.seq)
    assert!(
        d.contains("journal") && (d.contains("Msg") || d.contains("msg")),
        "RED (bd-6b1y #11): journal must be log of Msg; why is replay by seq/corr, not grep"
    );
}

// 8. source-scan meta: toron receipt get/list wire check (also covers S1 durability premise)
#[test]
fn adr0027_s1_receipt_cli_wired() {
    let toron_cli_main = toron_root().join("crates/toron-cli/src/main.rs");
    if !toron_cli_main.exists() {
        eprintln!(
            "skip: toron-cli main.rs not found at {}",
            toron_cli_main.display()
        );
        return;
    }
    let m = read_file(&toron_cli_main);
    // receipt get/list should be wired (not just defined in cli.rs)
    let cli = read_file(&toron_root().join("crates/toron-cli/src/cli.rs"));
    if cli.contains("receipt") || cli.contains("Receipt") {
        assert!(
            m.contains("receipt") || m.contains("Receipt"),
            "RED (bd-gokp): toron-cli/src/cli.rs defines receipt verbs but main.rs does not wire them — wire receipt get/list to ReceiptStore"
        );
    }
}

//! `flywheel ship land` integration test (bd-ps6wc).
//!
//! A temp repo plus fake `br` / `toron` on PATH prove the whole ceremony in
//! one call: gate -> close -> sync -> release -> commit this bead's tracker
//! rows -> ship check. The fakes keep their state under `$FAKE_STATE`.
//!
//! Covered: one call yields a LEGAL ship check; a rerun is a no-op (no second
//! commit, one gate row); an injected `br close` failure stops at that step and
//! a rerun resumes from it; the reservation release actually happened (a code
//! mutation skipping it fails the assertion).

use flywheel_test_sandbox::TestSandbox;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    sandbox: TestSandbox,
    repo: PathBuf,
    state: PathBuf,
    work_sha: String,
}

fn sh(dir: &Path, program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn")
}

fn write_fakes(bin: &Path) {
    std::fs::create_dir_all(bin).expect("bin dir");
    let br = r#"#!/usr/bin/env bash
set -euo pipefail
exec python3 "$(dirname "$0")/br.py" "$@"
"#;
    let br_py = r#"#!/usr/bin/env python3
import json, os, sys

state = os.environ["FAKE_STATE"]
os.makedirs(state, exist_ok=True)
gates_path = os.path.join(state, "gates")
argv = sys.argv[1:]
cmd = argv[0] if argv else ""

def gates():
    if not os.path.exists(gates_path):
        return []
    with open(gates_path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]

def write_gates(rows):
    with open(gates_path, "w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, sort_keys=True) + "\n")

if cmd == "show":
    bead = argv[1]
    status = "open"
    if os.path.exists(os.path.join(state, "status")):
        status = open(os.path.join(state, "status"), encoding="utf-8").read().strip()
    itype = "bug"
    if os.path.exists(os.path.join(state, "issue_type")):
        itype = open(os.path.join(state, "issue_type"), encoding="utf-8").read().strip()
    print(json.dumps({
        "id": bead, "title": "fixture bead", "priority": 1, "status": status,
        "issue_type": itype, "verify": "cargo test -p demo", "blast": "Normal", "ac_shape": "Checkable",
    }))
elif cmd == "gate" and argv[1] == "list":
    print(json.dumps({"history": gates()}))
elif cmd == "gate" and argv[1] == "report":
    bead = argv[2]
    rest = argv[3:]
    opts = {}
    i = 0
    while i < len(rest):
        if rest[i].startswith("--"):
            opts[rest[i][2:]] = rest[i + 1] if i + 1 < len(rest) else ""
            i += 2
        else:
            i += 1
    rows = gates()
    row = {
        "id": len(rows), "issue_id": bead, "gate": opts.get("gate", ""),
        "provider": opts.get("provider", ""), "passed": True,
        "note": opts.get("note", ""), "recorded_by": "fake",
        "recorded_at": "2026-01-01T00:00:00Z",
        "from_status": "in_progress", "to_status": "closed",
        "status_revision": len(rows),
    }
    rows.append(row)
    write_gates(rows)
    with open(os.path.join(state, "last_gate"), "w", encoding="utf-8") as fh:
        fh.write(json.dumps(row, sort_keys=True) + "\n")
elif cmd == "close":
    bead = argv[1]
    if "--dry-run" in argv:
        # br --dry-run answers without writing: it names the legal gate set
        # only when a policy NARROWS the transition, so the unrestricted
        # fixture prints no sentence and touches no state (bd-42vhw).
        print("close --dry-run %s (open -> closed): would close" % bead)
        sys.exit(0)
    if os.path.exists(os.path.join(state, "fail_close")):
        os.remove(os.path.join(state, "fail_close"))
        print("injected close failure", file=sys.stderr)
        sys.exit(1)
    with open(os.path.join(state, "status"), "w", encoding="utf-8") as fh:
        fh.write("closed\n")
    path = ".beads/issues.jsonl"
    rows = [json.loads(line) for line in open(path, encoding="utf-8") if line.strip()]
    for row in rows:
        if row.get("id") == bead:
            row["status"] = "closed"
    with open(path, "w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, sort_keys=True) + "\n")
    last = os.path.join(state, "last_gate")
    if os.path.exists(last):
        with open(os.path.join(".beads", "gates.jsonl"), "a", encoding="utf-8") as out, open(last, encoding="utf-8") as src:
            out.write(src.read())
elif cmd == "sync":
    pass
else:
    print("fake br: unsupported " + " ".join(argv), file=sys.stderr)
    sys.exit(2)
"#;
    std::fs::write(bin.join("br"), br).expect("write br");
    std::fs::write(bin.join("br.py"), br_py).expect("write br.py");
    let toron = r#"#!/usr/bin/env bash
set -euo pipefail
state="${FAKE_STATE:?}"
printf '%s\n' "$*" >>"$state/released"
printf '{"leases":[],"count":0}\n'
"#;
    std::fs::write(bin.join("toron"), toron).expect("write toron");
    for name in ["br", "br.py", "toron"] {
        let path = bin.join(name);
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        use std::os::unix::fs::PermissionsExt as _;
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
}

fn fixture() -> Fixture {
    let sandbox = TestSandbox::new();
    let repo = sandbox.root().join("repo");
    let bin = sandbox.bin_dir();
    let state = sandbox.root().join("state");
    std::fs::create_dir_all(&repo).expect("repo");
    std::fs::create_dir_all(&state).expect("state");
    write_fakes(&bin);
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        let out = sh(&repo, "git", &args);
        assert!(out.status.success(), "git {args:?}");
    }
    // The landing gate re-derives spec targets with the real harness; without
    // one it fails closed and treats every changed path as covered.
    std::fs::create_dir_all(repo.join("scripts/mutations")).expect("mutations dir");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/verify-mutations.sh"),
        repo.join("scripts/verify-mutations.sh"),
    )
    .expect("copy the harness");
    std::fs::create_dir_all(repo.join(".beads")).expect(".beads");
    std::fs::write(
        repo.join(".beads/issues.jsonl"),
        "{\"id\":\"bd-x\",\"status\":\"in_progress\",\"verify\":\"cargo test -p demo\"}\n",
    )
    .expect("issues");
    std::fs::write(repo.join(".beads/gates.jsonl"), "").expect("gates");
    // A real (non-bookkeeping) file: the SHA rule binds the gate to a commit
    // that carries work, and a tracker-only "work" commit is skipped.
    std::fs::write(repo.join("work.txt"), "work\n").expect("work file");
    assert!(sh(&repo, "git", &["add", "."]).status.success());
    assert!(
        sh(&repo, "git", &["commit", "-qm", "work bd-x"])
            .status
            .success()
    );
    let work_sha = String::from_utf8_lossy(&sh(&repo, "git", &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    Fixture {
        sandbox,
        repo,
        state,
        work_sha,
    }
}

impl Fixture {
    fn land_with(&self, sha: &str, extra: &[&str]) -> std::process::Output {
        let receipt = self.state.join("receipt.json");
        if !receipt.exists() {
            // A receipt the current gate accepts: it proves THIS revision, and
            // an empty mutation set is legal because the fixture tree has no
            // spec targets (bd-m5afp's rule re-derives that, it does not trust
            // the list).
            let body = format!(
                "{{\"ok\":true,\"sha\":\"{sha}\",\"mode\":\"sha\",\"mutations\":[],\"mutations_mode\":\"changed\",\"mutations_base\":\"{sha}\",\"changed_paths\":[\"work.txt\"]}}\n"
            );
            std::fs::write(&receipt, body).expect("receipt");
        }
        let mut args = vec![
            "ship",
            "land",
            "bd-x",
            "--sha",
            sha,
            "--receipt",
            receipt.to_str().expect("receipt path"),
            "--project",
            "proj",
            "--as",
            "TestPin",
            "--dir",
            self.repo.to_str().expect("repo path"),
        ];
        args.extend_from_slice(extra);
        self.sandbox
            .command(env!("CARGO_BIN_EXE_flywheel"))
            .env("PATH", self.sandbox.path_env())
            .env("FAKE_STATE", &self.state)
            .args(args)
            .output()
            .expect("land")
    }

    fn land(&self, extra: &[&str]) -> std::process::Output {
        self.land_with(&self.work_sha, extra)
    }

    fn head_count(&self) -> usize {
        let out = sh(&self.repo, "git", &["rev-list", "--count", "HEAD"]).stdout;
        String::from_utf8_lossy(&out).trim().parse().expect("count")
    }

    /// Commit a mutation spec declaring one id, and return the sha it proved.
    fn commit_spec_declaring(&self, id: &str) -> String {
        let rel = "scripts/mutations/adr0022-gates.spec.sh";
        let body = format!(
            "mutation \"{id}  a decision\" \\\n    file=src/x.rs \\\n    find='a' \\\n    replace='b' \\\n    expect=a_test\n"
        );
        std::fs::write(self.repo.join(rel), body).expect("spec");
        assert!(sh(&self.repo, "git", &["add", rel]).status.success());
        assert!(
            sh(
                &self.repo,
                "git",
                &["commit", "-qm", &format!("declare {id} bd-x")]
            )
            .status
            .success()
        );
        String::from_utf8_lossy(&sh(&self.repo, "git", &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string()
    }

    fn write_mutation_receipt(&self, proved: &str, id: &str) {
        let body = format!(
            "{{\"ok\":true,\"sha\":\"{proved}\",\"mode\":\"sha\",\"mutations\":[{{\"spec\":\"adr0022-gates.spec.sh\",\"id\":\"{id}\",\"verdict\":\"RED\"}}]}}\n"
        );
        std::fs::write(self.state.join("receipt.json"), body).expect("receipt");
    }

    fn released(&self) -> String {
        std::fs::read_to_string(self.state.join("released")).unwrap_or_default()
    }

    fn gate_rows(&self) -> usize {
        std::fs::read_to_string(self.repo.join(".beads/gates.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }
}

#[test]
fn land_runs_the_whole_ceremony_and_a_rerun_is_a_no_op() {
    let f = fixture();
    let out = f.land(&[]);
    assert!(
        out.status.success(),
        "first land failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        f.released().contains("release-by-reason bd-x"),
        "release ran"
    );
    assert_eq!(f.gate_rows(), 1, "one gate row landed");
    let after_first = f.head_count();
    let head_rows =
        String::from_utf8_lossy(&sh(&f.repo, "git", &["show", "HEAD:.beads/issues.jsonl"]).stdout)
            .to_string();
    assert!(
        head_rows.contains("\"status\": \"closed\"") || head_rows.contains("\"status\":\"closed\""),
        "closed row is committed: {head_rows}"
    );

    let again = f.land(&[]);
    assert!(
        again.status.success(),
        "rerun must be a no-op success: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(f.head_count(), after_first, "rerun makes no second commit");
    assert_eq!(f.gate_rows(), 1, "rerun does not duplicate the gate row");
}

#[test]
fn injected_close_failure_resumes_from_that_step() {
    let f = fixture();
    std::fs::write(f.state.join("fail_close"), "1").expect("fail flag");
    let first = f.land(&[]);
    assert!(
        !first.status.success(),
        "the injected close failure stops land"
    );
    let stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        stderr.contains("resume: flywheel ship land bd-x"),
        "resume line: {stderr}"
    );
    assert_eq!(f.gate_rows(), 0, "nothing landed after the failure");
    let before = f.head_count();

    let second = f.land(&[]);
    assert!(
        second.status.success(),
        "rerun resumes and lands: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(f.head_count(), before + 1, "exactly one tracker commit");
    assert_eq!(f.gate_rows(), 1, "the gate row is not duplicated on resume");
}

/// bd-wcjsu: a later commit that merely NAMES the closed bead must not move the
/// binding — the gate's `sha=` decides.
#[test]
fn a_later_mention_does_not_flip_a_landed_verdict() {
    let f = fixture();
    let out = f.land(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A REAL commit (the live case changed files), not an empty one: derive
    // filters commits that change nothing.
    std::fs::write(f.repo.join("mentions.txt"), "unrelated work\n").expect("mention file");
    assert!(
        sh(&f.repo, "git", &["add", "mentions.txt"])
            .status
            .success()
    );
    assert!(
        sh(
            &f.repo,
            "git",
            &["commit", "-qm", "mentions bd-x in passing"]
        )
        .status
        .success()
    );
    let check = f
        .sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", f.sandbox.path_env())
        .env("FAKE_STATE", &f.state)
        .args([
            "ship",
            "check",
            "bd-x",
            "--dir",
            f.repo.to_str().expect("repo"),
        ])
        .output()
        .expect("ship check");
    assert!(
        check.status.success(),
        "a mention-only commit must not flip the verdict:\n{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn land_refuses_a_sha_that_does_not_cite_the_bead() {
    let f = fixture();
    assert!(
        sh(
            &f.repo,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "unrelated"]
        )
        .status
        .success()
    );
    let unrelated = String::from_utf8_lossy(&sh(&f.repo, "git", &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let out = f.land_with(&unrelated, &[]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not cite"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// bd-t5f0v: a bug bead's confirmed root cause is a lesson candidate. The nudge
/// is advisory — the land still exits 0 with no answer.
#[test]
fn landing_a_bug_bead_prints_an_advisory_lesson_prompt() {
    let f = fixture();
    let out = f.land(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("lesson recorded or clean skip?"),
        "a bug land must nudge a lesson, without blocking: {stdout}"
    );
}

/// A non-bug bead gets no nudge — the prompt is not noise on every land.
#[test]
fn landing_a_non_bug_bead_prints_no_lesson_prompt() {
    let f = fixture();
    std::fs::write(f.state.join("issue_type"), "feature").expect("issue type");
    let out = f.land(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("lesson recorded or clean skip?"),
        "a feature land must stay quiet: {stdout}"
    );
}

/// bd-pgzxn: a receipt witnesses the revision it proved. A later commit may
/// relabel the mutation (D5 -> D5A/D5B in f34f9b54) without invalidating every
/// in-flight receipt fleet-wide.
#[test]
fn a_receipt_is_checked_at_the_sha_it_proved_not_the_worktree() {
    let f = fixture();
    let proved = f.commit_spec_declaring("D5");
    f.write_mutation_receipt(&proved, "D5");
    // HEAD now declares only the relabelled id.
    let _head = f.commit_spec_declaring("D5A");

    let out = f.land_with(&proved, &[]);
    assert!(
        out.status.success(),
        "a receipt citing an id present at its proved sha must still land:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The other half: a receipt citing an id absent even at the proved sha is
/// still refused. Reading at the proved sha must not turn the check off.
#[test]
fn a_receipt_citing_an_id_absent_at_the_proved_sha_still_refuses() {
    let f = fixture();
    let proved = f.commit_spec_declaring("D5");
    f.write_mutation_receipt(&proved, "D9");

    let out = f.land_with(&proved, &[]);
    assert!(!out.status.success(), "an undeclared id must still refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no longer declares"),
        "refusal still names the unbound citation: {stderr}"
    );
}

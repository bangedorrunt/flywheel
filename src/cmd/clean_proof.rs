// governance: ADR-0019 (close ceremony), bd-jp9z1.3.
//! Fail-fast ordering for `scripts/clean-proof.sh`.
//!
//! These are structure and subprocess tests, not unit tests of Rust code: the
//! rules they protect live in two shell scripts, and the failure mode they
//! prevent is a forty-minute proof that discovers a rotted needle. They run in
//! well under a second and need no compiler.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    // bd-b2yg7: these tests also run INSIDE verify-mutations' scratch exports.
    // A compile-time manifest dir can point at an export that has already been
    // cleaned up, so resolve it from the running test's environment instead.
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn read_script(name: &str) -> String {
    std::fs::read_to_string(repo_root().join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
}

/// One isolated fixture: a spec, its target, and whatever the target holds.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(target_body: &str, find: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("scripts/mutations")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/x.rs"), target_body).unwrap();
        std::fs::write(dir.path().join("src/t.rs"), "#[test]\nfn a_test() {}\n").unwrap();
        std::fs::write(
            dir.path().join("scripts/mutations/demo.spec.sh"),
            format!(
                "TEST_FILTER=(a_test)\nmutation \"D1  a decision\" \\\n    file=src/x.rs \\\n    find='{find}' \\\n    replace='other' \\\n    expect=a_test\n"
            ),
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new("bash")
            .arg(repo_root().join("scripts/verify-needles.sh"))
            .arg("--repo")
            .arg(self.path())
            .args(args)
            .output()
            .expect("run verify-needles")
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A find= that matches nothing is the NEEDLE-MISS clean-proof used to reach
/// forty minutes in. The lint must name the spec and the mutation id.
#[test]
fn the_needle_lint_fails_and_names_spec_and_id_when_a_needle_is_gone() {
    let fx = Fixture::new("nothing here\n", "vanished_needle");
    let out = fx.run(&[]);
    assert!(!out.status.success(), "a missing needle must fail the lint");
    let err = stderr(&out);
    assert!(
        err.contains("demo.spec.sh#D1"),
        "the failure names spec#id: {err}"
    );
    assert!(err.contains("occurs 0 time(s)"), "{err}");
}

/// Two matches means perl substitutes the first and the spec silently stops
/// covering the second site.
#[test]
fn the_needle_lint_fails_when_a_needle_occurs_twice() {
    let fx = Fixture::new("dup needle\nneedle again\n", "needle");
    let out = fx.run(&[]);
    assert!(!out.status.success(), "a doubled needle must fail the lint");
    let err = stderr(&out);
    assert!(err.contains("occurs 2 time(s)"), "{err}");
}

/// Exactly one occurrence is the contract the harness assumes.
#[test]
fn the_needle_lint_passes_on_exactly_one_occurrence() {
    let fx = Fixture::new("a needle and more\n", "needle");
    let out = fx.run(&[]);
    assert!(
        out.status.success(),
        "one occurrence must pass: {}",
        stderr(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("0 bad"), "{stdout}");
}

/// A spec that names a file the tree no longer has is the `mail_shim.rs` rot:
/// the lint must say so instead of letting the mutation run find out.
#[test]
fn the_needle_lint_reports_a_missing_target_file() {
    let fx = Fixture::new("a needle\n", "needle");
    std::fs::write(
        fx.path().join("scripts/mutations/demo.spec.sh"),
        "mutation \"D1  a decision\" \\\n    file=src/gone.rs \\\n    find='needle' \\\n    replace='other' \\\n    expect=a_test\n",
    )
    .unwrap();
    let out = fx.run(&[]);
    assert!(!out.status.success(), "a missing target must fail the lint");
    assert!(
        stderr(&out).contains("target does not exist: src/gone.rs"),
        "{}",
        stderr(&out)
    );
}

/// The cheapest check runs first: the needle lint, then the build, then the
/// lint gate, then the suite. Each later failure costs minutes more.
#[test]
fn clean_proof_runs_needles_then_build_then_clippy_then_suite() {
    let script = read_script("scripts/clean-proof.sh");
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("clean-proof.sh does not run {needle:?}"))
    };
    let needles = at("run_step verify-needles bash scripts/verify-needles.sh");
    let build = at("run_step build ");
    let clippy = at("run_step verify-clippy bash scripts/verify-clippy.sh");
    let changed = at("run_changed_file_tests || true");
    let suite = at("run_parallel_step suite ");
    assert!(
        needles < build,
        "the needle lint must run before anything is compiled"
    );
    assert!(
        build < clippy && clippy < changed && changed < suite,
        "clippy and changed-file tests must land before the suite starts"
    );
}

fn ordered_specs_source() -> String {
    let script = read_script("scripts/clean-proof.sh");
    let start = script
        .find("\nordered_specs() {")
        .expect("clean-proof.sh defines ordered_specs")
        + 1;
    let end = script[start..]
        .find("\n}\n")
        .expect("ordered_specs is a closed function")
        + start
        + 2;
    script[start..end].to_string()
}

/// Newest change first, so a spec edited in this landing fails before the
/// twenty-nine that have not moved. The proof loop must consume that order.
#[test]
fn mutation_specs_run_newest_change_first() {
    let script = read_script("scripts/clean-proof.sh");
    assert!(
        script.contains("done < <(ordered_specs)"),
        "the mutation loop must consume ordered_specs"
    );
    let function = ordered_specs_source();

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("scripts/mutations")).unwrap();
    for name in ["alpha.spec.sh", "beta.spec.sh"] {
        std::fs::write(
            dir.path().join("scripts/mutations").join(name),
            "# fixture\n",
        )
        .unwrap();
    }
    let recency = dir.path().join("recency.tsv");
    std::fs::write(&recency, "100\talpha.spec.sh\n200\tbeta.spec.sh\n").unwrap();
    std::fs::write(dir.path().join("ordered.sh"), &function).unwrap();

    let out = Command::new("bash")
        .arg("-c")
        .arg("SPEC_RECENCY=\"$1\"; . \"$2\"; ordered_specs")
        .arg("--")
        .arg(&recency)
        .arg(dir.path().join("ordered.sh"))
        .current_dir(dir.path())
        .output()
        .expect("run ordered_specs");
    assert!(
        out.status.success(),
        "ordered_specs must run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let order: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        order,
        vec![
            "scripts/mutations/beta.spec.sh".to_string(),
            "scripts/mutations/alpha.spec.sh".to_string(),
        ],
        "the most recently changed spec runs first"
    );
}

/// A proof of one sha must run THAT sha's driver (bd-jp9z1.3 follow-up).
/// Otherwise another lane's uncommitted edit to clean-proof.sh silently changes
/// what a receipt of an older sha checks, and a proof of a tree without a new
/// step dies on the working copy's version of it.
#[test]
fn a_sha_proof_runs_the_revisions_own_driver() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scripts = dir.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let real_driver = std::fs::read_to_string(repo_root().join("scripts/clean-proof.sh")).unwrap();
    let real_lib = std::fs::read_to_string(repo_root().join("scripts/proof-lib.sh")).unwrap();
    std::fs::write(scripts.join("proof-lib.sh"), &real_lib).unwrap();
    // The committed driver announces itself once it is actually running.
    let marker = "echo 'REV-DRIVER-RAN' >&2\n";
    let at = real_driver
        .find("\n# shellcheck source=proof-lib.sh")
        .expect("the snapshot block is followed by the library source line")
        + 1;
    let committed = format!("{}{}{}", &real_driver[..at], marker, &real_driver[at..]);
    std::fs::write(scripts.join("clean-proof.sh"), &committed).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "T"],
        vec!["add", "scripts"],
        vec!["commit", "-q", "-m", "driver at the proven rev"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let rev = String::from_utf8(
        Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    // The working tree now carries a driver WITHOUT the marker: the copy a
    // buggy snapshot would run.
    std::fs::write(scripts.join("clean-proof.sh"), &real_driver).unwrap();

    let lock = dir.path().join("clean-proof.lock");
    // A proof of one sha must be judged on its own, never on the variables a
    // proof that is CURRENTLY RUNNING exported into this process (that is how
    // this test first failed inside a real proof: CLEAN_PROOF_SNAPSHOTTED made
    // the child skip the driver resolution it is here to prove). Poison this
    // process with them so the fence is proved on every run, not only when an
    // outer proof happens to be in flight.
    unsafe {
        std::env::set_var("CLEAN_PROOF_SNAPSHOTTED", "1");
        std::env::set_var("CLEAN_PROOF_DRIVER_SHA", "poison");
    }
    let out = Command::new("bash")
        .arg(scripts.join("clean-proof.sh"))
        .args(["--sha", &rev, "--lock-hold-test", "0"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path().join("home"))
        .env("TMPDIR", dir.path())
        .env("CLEAN_PROOF_LOCK_FILE", &lock)
        .output()
        .expect("run clean-proof");
    unsafe {
        std::env::remove_var("CLEAN_PROOF_SNAPSHOTTED");
        std::env::remove_var("CLEAN_PROOF_DRIVER_SHA");
    }
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains(&format!("driver from {rev}")),
        "the driver source sha is named: {err}"
    );
    assert!(
        err.contains("REV-DRIVER-RAN"),
        "the revision's driver actually ran, not the working tree's: {err}"
    );
}

/// A fixture that the cache script can key: a spec, its target, the test that
/// the mutation names, the harness itself, a lockfile and a sibling pin.
struct CacheFixture {
    dir: tempfile::TempDir,
}

impl CacheFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("scripts/mutations")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // Embedded, not copied from the checkout: include_str! makes both
        // scripts build inputs, so a mutation to either forces a rebuild and
        // the fixture can never run a stale binary's idea of them.
        std::fs::write(
            root.join("scripts/verify-mutations.sh"),
            include_str!("../../scripts/verify-mutations.sh"),
        )
        .unwrap();
        std::fs::write(
            root.join("scripts/proof-lib.sh"),
            include_str!("../../scripts/proof-lib.sh"),
        )
        .unwrap();
        std::fs::write(
            root.join("scripts/mutation-cache.py"),
            include_str!("../../scripts/mutation-cache.py"),
        )
        .unwrap();
        std::fs::write(
            root.join("scripts/mutations/demo.spec.sh"),
            "CARGO_ARGS=(--lib)\nTEST_FILTER=(demo)\n\nmutation \"D1  a decision\" \\\n    file=src/x.rs \\\n    find='the_needle' \\\n    replace='other' \\\n    expect=a_test\n",
        )
        .unwrap();
        std::fs::write(root.join("src/x.rs"), "fn f() { the_needle }\n").unwrap();
        std::fs::write(root.join("src/t.rs"), "#[test]\nfn a_test() {}\n").unwrap();
        std::fs::write(root.join("Cargo.lock"), "[[package]]\nname = \"demo\"\n").unwrap();
        std::fs::write(
            root.join("proof.toml"),
            "[[sibling]]\npath = \"../toron\"\nrev = \"1111111111111111111111111111111111111111\"\n",
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn key(&self, driver: &str) -> String {
        let out = Command::new("python3")
            .arg(self.path().join("scripts/mutation-cache.py"))
            .args([
                "key",
                "--root",
                &self.path().display().to_string(),
                "--spec",
                "scripts/mutations/demo.spec.sh",
                "--driver",
                driver,
            ])
            .output()
            .expect("run key");
        assert!(
            out.status.success(),
            "key failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn lookup(&self, key: &str, expect: &Path) -> Output {
        Command::new("python3")
            .arg(self.path().join("scripts/mutation-cache.py"))
            .args([
                "lookup",
                "--dir",
                &self.path().join("cache").display().to_string(),
                "--key",
                key,
                "--expect",
                &expect.display().to_string(),
            ])
            .output()
            .expect("run lookup")
    }

    fn expect_file(&self) -> PathBuf {
        let path = self.path().join("expect.tsv");
        std::fs::write(&path, "D1\ta_test\t0\n").unwrap();
        path
    }
}

/// bd-kddv7: the key covers only what a verdict depends on — the mutated
/// fn, the expect test fn, the spec, the harness and the lockfile. An edit
/// to an unrelated fn in the same file must NOT drop cached verdicts; sibling
/// pins and driver scripts must NOT drop them either.
#[test]
fn the_cache_key_scopes_to_the_mutated_and_witness_fns() {
    let fx = CacheFixture::new();
    let base = fx.key("driver-sha");
    assert_eq!(base.len(), 64, "the key is a sha256 hex string");

    let same = |label: &str| assert_eq!(base, fx.key("driver-sha"), "{label}");
    let moved = |label: &str| assert_ne!(base, fx.key("driver-sha"), "{label}");
    let write = |rel: &str, body: &str| std::fs::write(fx.path().join(rel), body).unwrap();

    same("baseline");
    write("src/x.rs", "fn f() { the_needle }\nfn unrelated() { 1 }\n");
    same("an unrelated fn in the target file");
    write("src/t.rs", "#[test]\nfn a_test() {}\nfn other_test() {}\n");
    same("an unrelated fn in the test file");
    write(
        "proof.toml",
        "[[sibling]]\npath = \"../toron\"\nrev = \"2222222222222222222222222222222222222222\"\n",
    );
    same("a sibling pin bump");
    write("scripts/clean-proof.sh", "driver v2\n");
    same("a driver script edit");
    same("a driver sha");
    assert_eq!(
        base,
        fx.key("another-driver"),
        "a commit sha alone is not an input"
    );

    // Now the inputs that MUST move the key, each from the current state.
    let before = fx.key("driver-sha");
    write(
        "src/x.rs",
        "fn f() { other_needle }\nfn unrelated() { 1 }\n",
    );
    assert_ne!(
        before,
        fx.key("driver-sha"),
        "the mutated fn must move the key"
    );

    let before = fx.key("driver-sha");
    write(
        "src/t.rs",
        "#[test]\nfn a_test() { let _ = 42; }\nfn other_test() {}\n",
    );
    assert_ne!(
        before,
        fx.key("driver-sha"),
        "the expect test fn must move the key"
    );

    let before = fx.key("driver-sha");
    write(
        "scripts/mutations/demo.spec.sh",
        "CARGO_ARGS=(--lib)\nTEST_FILTER=(demo)\n\n# note\nmutation \"D1  a decision\" \\\n    file=src/x.rs \\\n    find='other_needle' \\\n    replace='something' \\\n    expect=a_test\n",
    );
    assert_ne!(before, fx.key("driver-sha"), "the spec must move the key");

    moved("the spec byte relative to baseline");
    let before = fx.key("driver-sha");
    let harness = std::fs::read_to_string(fx.path().join("scripts/verify-mutations.sh"))
        .unwrap()
        .replace(
            "VERDICT_VACUOUS=\"VACUOUS\"",
            "VERDICT_VACUOUS=\"VACUOUS2\"",
        );
    write("scripts/verify-mutations.sh", &harness);
    assert_ne!(
        before,
        fx.key("driver-sha"),
        "the harness must move the key"
    );

    let before = fx.key("driver-sha");
    write("Cargo.lock", "[[package]]\nname = \"demo2\"\n");
    assert_ne!(
        before,
        fx.key("driver-sha"),
        "the lockfile must move the key"
    );
}

/// A hit needs the key, every declared id at its expected verdict, and a
/// witness receipt that is still green.
#[test]
fn a_cached_spec_hits_only_on_the_same_key_with_a_green_witness() {
    let fx = CacheFixture::new();
    let key = fx.key("driver-sha");
    let witness = fx.path().join("witness.json");
    std::fs::write(&witness, r#"{"ok":true,"sha":"deadbeef"}"#).unwrap();
    let rows = fx.path().join("rows.jsonl");
    std::fs::write(
        &rows,
        "{\"spec\":\"demo.spec.sh\",\"id\":\"D1\",\"expect_test\":\"a_test\",\"expect_green\":false,\"verdict\":\"RED\"}\n",
    )
    .unwrap();
    let stored = Command::new("python3")
        .arg(fx.path().join("scripts/mutation-cache.py"))
        .args([
            "store",
            "--dir",
            &fx.path().join("cache").display().to_string(),
            "--key",
            &key,
            "--spec",
            "scripts/mutations/demo.spec.sh",
            "--rows",
            &rows.display().to_string(),
            "--witness",
            &witness.display().to_string(),
            "--witness-sha",
            "deadbeef",
        ])
        .output()
        .expect("store");
    assert!(stored.status.success());

    let expect = fx.expect_file();
    let hit = fx.lookup(&key, &expect);
    assert!(
        hit.status.success(),
        "a matching key with a green witness hits"
    );
    let stdout = String::from_utf8_lossy(&hit.stdout);
    assert!(
        stdout.contains("\"cached\""),
        "the row names its witness: {stdout}"
    );

    // A different key is a miss even with the entry still on disk.
    assert_eq!(fx.lookup(&"0".repeat(64), &expect).status.code(), Some(3));

    // The same key with a verdict that is not the expected RED is a miss: a
    // cached GREEN cannot stand in for a red witness.
    let green_key = "1".repeat(64);
    let green_rows = fx.path().join("green.jsonl");
    std::fs::write(
        &green_rows,
        "{\"spec\":\"demo.spec.sh\",\"id\":\"D1\",\"expect_test\":\"a_test\",\"expect_green\":false,\"verdict\":\"GREEN\"}\n",
    )
    .unwrap();
    let stored_green = Command::new("python3")
        .arg(fx.path().join("scripts/mutation-cache.py"))
        .args([
            "store",
            "--dir",
            &fx.path().join("cache").display().to_string(),
            "--key",
            &green_key,
            "--spec",
            "scripts/mutations/demo.spec.sh",
            "--rows",
            &green_rows.display().to_string(),
            "--witness",
            &witness.display().to_string(),
            "--witness-sha",
            "deadbeef",
        ])
        .output()
        .expect("store green");
    assert!(stored_green.status.success());
    assert_eq!(fx.lookup(&green_key, &expect).status.code(), Some(3));

    // The witness going away invalidates the hit: nothing outlives its evidence.
    std::fs::remove_file(&witness).unwrap();
    assert_eq!(fx.lookup(&key, &expect).status.code(), Some(3));
}

/// The key recomputed from a git revision must equal the key computed in the
/// checkout — that equality is what lets ship land verify a cached row.
#[test]
fn the_key_from_a_revision_equals_the_key_from_its_checkout() {
    let fx = CacheFixture::new();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "T"],
        vec!["add", "."],
        vec!["commit", "-q", "-m", "fixture"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(fx.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let rev = String::from_utf8(
        Command::new("git")
            .current_dir(fx.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let from_root = fx.key("driver-sha");
    let from_rev = Command::new("python3")
        .arg(fx.path().join("scripts/mutation-cache.py"))
        .args([
            "key",
            "--proven",
            &rev,
            "--repo",
            &fx.path().display().to_string(),
            "--spec",
            "scripts/mutations/demo.spec.sh",
            "--driver",
            "driver-sha",
        ])
        .output()
        .expect("key from rev");
    assert!(
        from_rev.status.success(),
        "{}",
        String::from_utf8_lossy(&from_rev.stderr)
    );
    assert_eq!(
        from_root,
        String::from_utf8_lossy(&from_rev.stdout).trim(),
        "the same tree must key the same from a checkout and from git"
    );
}

/// bd-b2yg7: the mutation inputs are content, not a stale pin or a commit id.
/// The shell fixture drives the real proof-lib helpers and the real mutation
/// harness; these pins keep clean-proof wired to them.
#[test]
fn clean_proof_scopes_mutations_and_shares_one_target_dir() {
    let script = read_script("scripts/clean-proof.sh");
    assert!(
        script.contains("proof_green_base \"$ROOT\" \"$PROVEN_SHA\" \"$RECEIPT_ARCHIVE\""),
        "the mutation base must come from the newest green receipt"
    );
    assert!(
        script.contains("VERIFY_MUTATIONS_TARGET_DIR=$TMP/target"),
        "one proof-owned target dir must be shared by every mutation run"
    );
    assert!(
        script.contains("PROOF_RECEIPT_ARCHIVE=\"$RECEIPT_ARCHIVE\""),
        "green receipts must be archived for the next proof"
    );
    let harness = read_script("scripts/verify-mutations.sh");
    assert!(
        harness.contains("${VERIFY_MUTATIONS_TARGET_DIR:-$export_root/target}"),
        "the harness must honour a caller-owned target dir"
    );
    let toml = read_script("proof.toml");
    let active_pin = toml
        .lines()
        .any(|line| line.trim_start().starts_with("base ="));
    assert!(
        !active_pin,
        "proof.toml must not carry a stale manual base pin"
    );
}

/// Real receipts stamp proofs as compact ISO ("20261118T041942Z"), not as an
/// integer. The base must still be found; a numeric-only comparison returns
/// empty and leaves every proof running every spec.
#[test]
fn proof_green_base_accepts_iso_stamped_receipts() {
    let root = repo_root();
    let script = format!(
        r#"set -euo pipefail
d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
git init -q "$d/repo"
git -C "$d/repo" config user.email proof@test
git -C "$d/repo" config user.name proof
echo one >"$d/repo/f"
git -C "$d/repo" add f
git -C "$d/repo" commit -qm one
ancestor=$(git -C "$d/repo" rev-parse HEAD)
echo two >"$d/repo/f"
git -C "$d/repo" add f
git -C "$d/repo" commit -qm two
proven=$(git -C "$d/repo" rev-parse HEAD)
mkdir -p "$d/receipts"
printf '%s' '{{"ok":true,"sha":"'"$ancestor"'","stamp":"20261118T041942Z","mutations_mode":"changed","mutations":[{{"verdict":"RED"}},{{"verdict":"DEAD-CONFIRMED"}}]}}' >"$d/receipts/r.json"
. "{root}/scripts/proof-lib.sh"
proof_green_base "$d/repo" "$proven" "$d/receipts"
"#,
        root = root.display()
    );
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(root)
        .output()
        .expect("run proof_green_base with an ISO stamp");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        !got.is_empty() && got.len() == 40,
        "an ISO-stamped green receipt must yield a base sha, got '{got}'"
    );
}

#[test]
fn proof_mutation_inputs_fixture_passes() {
    let out = std::process::Command::new("bash")
        .arg(format!(
            "{}/scripts/test-proof-mutation-inputs.sh",
            repo_root().display()
        ))
        .current_dir(repo_root())
        .output()
        .expect("run scripts/test-proof-mutation-inputs.sh");
    assert!(
        out.status.success(),
        "scripts/test-proof-mutation-inputs.sh failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// bd-2gxup: a stale Cargo.lock must fail the proof's build step, not drift
/// into the mutation cache key after the build rewrites it.
#[test]
fn clean_proof_builds_with_a_locked_lockfile() {
    let script = read_script("scripts/clean-proof.sh");
    assert!(
        script.contains("build --workspace --all-targets --locked"),
        "the proof build must refuse a stale Cargo.lock at the build step"
    );
}

/// The cache is only real if the proof passes a key and stores the witness
/// after the receipt exists. Both fail silently otherwise: everything runs and
/// nobody notices the cache never fills.
#[test]
fn clean_proof_computes_and_passes_a_cache_key() {
    let script = read_script("scripts/clean-proof.sh");
    assert!(
        script.contains("mutation-cache.py key --proven \"$PROVEN_SHA\" --repo \"$ROOT\""),
        "the proof computes each spec's key from the proven rev, as ship land does"
    );
    assert!(
        script.contains("--cache-key \"$mut_key\""),
        "the key reaches the harness"
    );
    assert!(
        script.contains("mutation-cache.py store"),
        "a freshly proved spec is stored with this receipt as its witness"
    );
    assert!(
        script.contains("[ \"$MUTATIONS_MODE\" != full ]"),
        "a full run bypasses the cache"
    );
}

/// The beads-ledger guard reads a close CLAIM from a structured trailer, never
/// from a bead id that merely appears in prose (bd-sy7vn). Its own self-test
/// pins both directions; a subject like "close the duplicates against bd-x"
/// must not read as a claim on bd-x.
#[test]
fn the_beads_ledger_guard_reads_trailers_not_prose() {
    let out = Command::new("bash")
        .arg(repo_root().join("scripts/check-beads-ledger.sh"))
        .arg("--self-test")
        .output()
        .expect("run the ledger guard self-test");
    assert!(
        out.status.success(),
        "ledger guard self-test failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// bd-n21ml: lift the mailer out of the script so a fixture receipt can be
/// routed without running a forty-minute proof.
fn mail_red_owners_source() -> String {
    let script = read_script("scripts/clean-proof.sh");
    let start = script
        .find("\nmail_red_owners() {")
        .expect("clean-proof.sh defines mail_red_owners")
        + 1;
    let end = script[start..]
        .find("\n}\n")
        .expect("mail_red_owners is a closed function")
        + start
        + 2;
    script[start..end].to_string()
}

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = crate::git_identity::command()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn red_rows_mail_the_owner_with_log_and_rerun() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    git_in(repo, &["init", "-q"]);
    git_in(repo, &["config", "user.email", "t@example.com"]);
    git_in(repo, &["config", "user.name", "Test"]);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join("scripts/mutations")).unwrap();
    std::fs::write(repo.join("src/owned.rs"), "fn x() {}\n").unwrap();
    std::fs::write(repo.join("src/owned2.rs"), "fn y() {}\n").unwrap();
    std::fs::write(
        repo.join("scripts/mutations/demo.spec.sh"),
        "# fixture\nfile=src/owned.rs\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("scripts/mutations/demo2.spec.sh"),
        "# fixture\nfile=src/owned2.rs\n",
    )
    .unwrap();
    git_in(
        repo,
        &["add", "src/owned.rs", "scripts/mutations/demo.spec.sh"],
    );
    git_in(repo, &["commit", "-q", "-m", "fix: bd-owner1 demo"]);
    git_in(
        repo,
        &["add", "src/owned2.rs", "scripts/mutations/demo2.spec.sh"],
    );
    git_in(repo, &["commit", "-q", "-m", "fix: bd-owner2 demo"]);

    let bin = repo.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let br = bin.join("br");
    std::fs::write(
        &br,
        "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in bd-owner1) printf '%s' '{\"assignee\":\"CrewOwner\"}'; exit 0;; bd-owner2) printf '%s' '{\"assignee\":\"CrewTwo\"}'; exit 0;; esac; done\nprintf '%s' '{}'\n",
    )
    .unwrap();
    let toron = bin.join("toron");
    std::fs::write(
        &toron,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TORON_LOG\"\n",
    )
    .unwrap();
    for shim in [&br, &toron] {
        let mut perms = std::fs::metadata(shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(shim, perms).unwrap();
    }

    let receipt = repo.join("receipt.json");
    std::fs::write(
        &receipt,
        r#"{"ok":false,"logs":"/logs","mutations":[{"spec":"demo.spec.sh","id":"D1","expect_test":"a_test","verdict":"RED"},{"spec":"demo2.spec.sh","id":"V1","expect_test":"b_test","verdict":"VACUOUS"}]}"#,
    )
    .unwrap();
    let mail_log = dir.path().join("mail.log");
    let function = mail_red_owners_source();
    let sourced = dir.path().join("mailfn.sh");
    std::fs::write(&sourced, &function).unwrap();

    let run = || {
        Command::new("bash")
            .arg("-c")
            .arg(". \"$1\"; mail_red_owners \"$2\" \"$3\" flywheel Flywheel")
            .arg("--")
            .arg(&sourced)
            .arg(&receipt)
            .arg(repo)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("TORON_LOG", &mail_log)
            .env("AGENT_NAME", "CaptainPin")
            .current_dir(repo)
            .output()
            .expect("run mail_red_owners")
    };

    let out = run();
    assert!(
        out.status.success(),
        "mailer must not fail the proof: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mailed = std::fs::read_to_string(&mail_log).unwrap_or_default();
    assert!(
        !mailed.contains("--to CrewOwner"),
        "a RED row is a caught mutant, not a failure: {mailed}"
    );
    assert!(
        mailed.contains("--to CrewTwo"),
        "the VACUOUS row's owner got the mail: {mailed}"
    );
    assert!(
        mailed.contains("/logs/mutations-demo2.spec.sh.log"),
        "log path: {mailed}"
    );
    assert!(
        mailed.contains(
            "bash scripts/verify-mutations.sh scripts/mutations/demo2.spec.sh --cargo mbx"
        ),
        "exact rerun command: {mailed}"
    );

    std::fs::write(&receipt, r#"{"ok":true,"logs":"/logs","mutations":[]}"#).unwrap();
    std::fs::write(&mail_log, "").unwrap();
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&mail_log).unwrap_or_default(),
        "",
        "a green proof mails nobody about failures"
    );

    // bd-1czzw follow-up: the git author ("Test") is not a pin and no bead
    // assignee resolves, so the mail must go to the captain, never the author.
    std::fs::write(&br, "#!/bin/sh\nprintf '%s' '{}'\n").unwrap();
    std::fs::write(
        &receipt,
        r#"{"ok":false,"logs":"/logs","mutations":[{"spec":"demo.spec.sh","id":"D1","expect_test":"a_test","verdict":"VACUOUS"}]}"#,
    )
    .unwrap();
    std::fs::write(&mail_log, "").unwrap();
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mailed = std::fs::read_to_string(&mail_log).unwrap_or_default();
    assert!(
        mailed.contains("--to CaptainPin"),
        "captain is the fallback: {mailed}"
    );
    assert!(
        !mailed.contains("--to Test") && !mailed.contains("--to bangedorrunt"),
        "a git author is never a mailing identity: {mailed}"
    );

    // A failed SIBLING step resolves its owner from the sibling's own commit,
    // and the mail body is capped so a long test dump cannot break gift-wrap.
    std::fs::write(
        &br,
        "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in bd-owner2) printf '%s' '{\"assignee\":\"CrewTwo\"}'; exit 0;; esac; done\nprintf '%s' '{}'\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&br).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&br, perms).unwrap();
    let sib = dir.path().join("sib");
    std::fs::create_dir_all(&sib).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "s@example.com"],
        vec!["config", "user.name", "S"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&sib)
                .args(&args)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(sib.join("x"), "x").unwrap();
    for args in [
        vec!["add", "x"],
        vec!["commit", "-qm", "bd-owner2 sibling fix"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&sib)
                .args(&args)
                .status()
                .unwrap()
                .success()
        );
    }
    let srev = String::from_utf8(
        Command::new("git")
            .current_dir(&sib)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let long = "x".repeat(2500);
    std::fs::write(
        &receipt,
        format!(
            "{{\"ok\":false,\"failed_step\":\"sibling-sib-tests\",\"siblings\":[{{\"path\":\"sib\",\"rev\":\"{srev}\"}}],\"logs\":\"/logs\",\"mutations\":[{{\"spec\":\"demo.spec.sh\",\"id\":\"D1\",\"expect_test\":\"{long}\",\"verdict\":\"VACUOUS\"}}]}}"
        ),
    )
    .unwrap();
    std::fs::write(&mail_log, "").unwrap();
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mailed = std::fs::read_to_string(&mail_log).unwrap_or_default();
    assert!(
        mailed.contains("--to CrewTwo"),
        "sibling owner resolved: {mailed}"
    );
    assert!(
        mailed.contains("(truncated"),
        "a long failure dump is capped before mailing"
    );
    assert!(
        !mailed.contains(&long),
        "the full test dump never reaches the mail"
    );
}

fn sibling_tests_source() -> String {
    let script = read_script("scripts/clean-proof.sh");
    let start = script
        .find("\nsibling_tests() {")
        .expect("clean-proof.sh defines sibling_tests")
        + 1;
    let end = script[start..]
        .find("\n}\n")
        .expect("sibling_tests is a closed function")
        + start
        + 2;
    script[start..end].to_string()
}

/// bd-lfii3: a red sibling suite fails the proof at a step that names the
/// sibling and carries the sibling test's own failure line.
#[test]
fn a_failing_sibling_suite_fails_the_step_naming_it() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let tmp = dir.path();
    std::fs::create_dir_all(tmp.join("toron")).unwrap();
    std::fs::write(
        tmp.join("toron/Cargo.toml"),
        "[package]\nname = \"toron\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let bin = tmp.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mbx = bin.join("mbx");
    std::fs::write(
        &mbx,
        "#!/bin/sh\nif [ \"$1\" = test ]; then echo \"$*\" >> \"$ARGS_LOG\"; echo 'test catalog_shapes ... FAILED'; echo 'test tools_list_fresh ... FAILED'; exit 1; fi\nexit 0\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&mbx).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&mbx, perms).unwrap();

    let sourced = tmp.join("sibling.sh");
    std::fs::write(&sourced, sibling_tests_source()).unwrap();
    let driver = tmp.join("driver.sh");
    std::fs::write(
        &driver,
        r#"set -u
TMP="$1"; MBX=("$2"); STEPS_LOG="$3"; FAILED_STEP=""
. "$4"
run_step() {
  local name="$1"; shift
  echo "$name" >> "$STEPS_LOG"
  "$@" >> "$STEPS_LOG" 2>&1 || FAILED_STEP="$name"
  return 0
}
sibling_tests <<< "toron"
echo "FAILED=$FAILED_STEP"
"#,
    )
    .unwrap();
    let steps_log = tmp.join("steps.log");
    let args_log = tmp.join("args.log");

    let out = Command::new("bash")
        .arg(&driver)
        .arg(tmp)
        .arg(&mbx)
        .arg(&steps_log)
        .arg(&sourced)
        .env("ARGS_LOG", &args_log)
        .output()
        .expect("run sibling_tests");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(&steps_log).unwrap_or_default();
    assert!(
        log.contains("sibling-toron-tests"),
        "step names sibling: {log}"
    );
    assert!(
        log.contains("catalog_shapes"),
        "first sibling red carried: {log}"
    );
    assert!(
        log.contains("tools_list_fresh"),
        "every red sibling test is listed, not just the first: {log}"
    );
    assert!(
        std::fs::read_to_string(&args_log)
            .unwrap_or_default()
            .contains("--no-fail-fast"),
        "the sibling suite asks for all failures"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("FAILED=sibling-toron-tests"),
        "failed step recorded: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    std::fs::write(&mbx, "#!/bin/sh\necho 'test result: ok'\nexit 0\n").unwrap();
    let mut perms = std::fs::metadata(&mbx).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&mbx, perms).unwrap();
    std::fs::write(&steps_log, "").unwrap();
    let out = Command::new("bash")
        .arg(&driver)
        .arg(tmp)
        .arg(&mbx)
        .arg(&steps_log)
        .arg(&sourced)
        .output()
        .expect("run sibling_tests green");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("FAILED="),
        "a green sibling does not fail the proof: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

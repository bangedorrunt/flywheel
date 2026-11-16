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
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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
        std::fs::write(
            dir.path().join("scripts/mutations/demo.spec.sh"),
            format!(
                "mutation \"D1  a decision\" \\\n    file=src/x.rs \\\n    find='{find}' \\\n    replace='other' \\\n    expect=a_test\n"
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
    let suite = at("run_step suite ");
    assert!(
        needles < build,
        "the needle lint must run before anything is compiled"
    );
    assert!(
        build < clippy && clippy < suite,
        "a clippy failure must land before the suite starts"
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

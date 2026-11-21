//! bd-j1g64: the remainder of bd-aberm/bd-himh8 — a temp clone with each
//! repo's hook installed must refuse a plain git commit with the flywheel
//! commit hint. The nonce path is already proven by src/cmd/commit.rs.
//! Every built-binary spawn goes through TestSandbox (test-sandbox law).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flywheel_test_sandbox::TestSandbox;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git")
}

fn scratch(tag: &str, hook_source: &Path, root: &Path) -> PathBuf {
    let dir = root.join(tag);
    std::fs::create_dir_all(dir.join("scripts/hooks")).unwrap();
    assert!(git(&dir, &["init", "-q"]).status.success());
    assert!(
        git(&dir, &["config", "user.email", "test@example.com"])
            .status
            .success()
    );
    assert!(git(&dir, &["config", "user.name", "Test"]).status.success());
    std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
    assert!(git(&dir, &["add", "seed.txt"]).status.success());
    assert!(git(&dir, &["commit", "-qm", "seed"]).status.success());
    std::fs::copy(hook_source, dir.join("scripts/hooks/pre-commit")).unwrap();
    // The hook's fence is not what this test proves; a no-op keeps the
    // nonce-enabled arm fast and offline.
    std::fs::write(dir.join("scripts/fence.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for rel in ["scripts/hooks/pre-commit", "scripts/fence.sh"] {
            let p = dir.join(rel);
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    assert!(
        git(&dir, &["config", "core.hooksPath", "scripts/hooks"])
            .status
            .success()
    );
    dir
}

fn plain_commit(dir: &Path) -> String {
    std::fs::write(dir.join("plain.txt"), "plain\n").unwrap();
    assert!(git(dir, &["add", "plain.txt"]).status.success());
    let out = git(dir, &["commit", "-qm", "plain"]);
    assert!(!out.status.success(), "plain git commit must be refused");
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn flywheel_hook_refuses_a_plain_commit_with_the_hint() {
    let sandbox = TestSandbox::new();
    let dir = scratch(
        "flywheel",
        &root().join("scripts/hooks/pre-commit"),
        sandbox.root(),
    );
    let err = plain_commit(&dir);
    assert!(err.contains("flywheel commit"), "{err}");
    assert!(err.contains("shared index"), "{err}");
}

#[test]
fn toron_hook_refuses_a_plain_commit_with_the_repo_hint() {
    let sandbox = TestSandbox::new();
    let hook = root().join("../toron/scripts/hooks/pre-commit");
    let dir = scratch("toron", &hook, sandbox.root());
    let err = plain_commit(&dir);
    assert!(err.contains("flywheel commit"), "{err}");
    assert!(err.contains("--repo ../toron"), "{err}");
}

#[test]
fn a_stale_staged_entry_is_reported_not_silently_healed() {
    // bd-himh8 heals a stale stage; bd-j1g64 requires the unique case to be
    // reported. The built binary runs under TestSandbox: no ambient HOME,
    // XDG dirs, toron store or routing env reaches it.
    let sandbox = TestSandbox::new();
    let dir = scratch(
        "stale",
        &root().join("scripts/hooks/pre-commit"),
        sandbox.root(),
    );
    std::fs::write(dir.join("unique.txt"), "a neighbour's work\n").unwrap();
    assert!(git(&dir, &["add", "unique.txt"]).status.success());
    let path = format!(
        "{}:{}",
        sandbox.bin_dir().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = sandbox
        .command(env!("CARGO_BIN_EXE_flywheel"))
        .env("PATH", path)
        .current_dir(&dir)
        .args([
            "commit",
            "seed.txt",
            "-m",
            "bd-j1g64 stale report",
            "--no-reservation-check",
        ])
        .env("FLYWHEEL_COMMIT_TOKEN", "nonce")
        .env("FLYWHEEL_COMMIT_OPERATOR", "0")
        .output()
        .expect("flywheel commit");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("unique.txt"),
        "the untouched staged path must be named: {text}"
    );
}

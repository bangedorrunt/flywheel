//! bd-npaqu: the commit fence must lint the tree it commits, not the shared
//! working copy where another lane may have an unrelated dirty edit.

use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn export_tree(repo: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    let tree = String::from_utf8(
        Command::new("git")
            .args(["-C"])
            .arg(repo)
            .args(["write-tree"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let archive = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .arg("archive")
        .arg(tree.trim())
        .output()
        .unwrap();
    assert!(archive.status.success());
    let mut child = Command::new("tar")
        .args(["-x", "-C"])
        .arg(dest)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&archive.stdout)
        .unwrap();
    assert!(child.wait().unwrap().success());
}

fn needle_check(repo: &Path) -> std::process::Output {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    Command::new("bash")
        .arg(root.join("scripts/verify-needles.sh"))
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("verify-needles")
}

#[test]
fn commit_needles_read_the_index_tree() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fence = std::fs::read_to_string(root.join("scripts/fence.sh")).unwrap();
    assert!(
        fence.contains("bash scripts/verify-needles.sh --repo \"$DEST\""),
        "the fence must lint the exported commit tree, not the working copy"
    );

    let tmp = std::env::temp_dir().join(format!("fw-commit-needles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let repo = tmp.join("repo");
    std::fs::create_dir_all(repo.join("scripts/mutations")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(
        repo.join("scripts/mutations/fixture.spec.sh"),
        "CARGO_ARGS=(-p flywheel --bin flywheel)\n\
         mutation \"F1  guard gone\" file=src/lib.rs expect= find=needle replace=MUT\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/lib.rs"), "needle\n").unwrap();
    std::fs::write(repo.join("README.md"), "seed\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "seed"]);

    // Another lane dirties an unrelated target in the working tree. A fence
    // over the index tree must not see it.
    std::fs::write(repo.join("src/lib.rs"), "dirty\n").unwrap();
    let clean = tmp.join("clean");
    export_tree(&repo, &clean);
    let out = needle_check(&clean);
    assert!(
        out.status.success(),
        "a dirty working tree must not refuse a commit: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The same edit staged for commit is exactly what must refuse it.
    git(&repo, &["add", "src/lib.rs"]);
    let broken = tmp.join("broken");
    export_tree(&repo, &broken);
    let out = needle_check(&broken);
    assert!(
        !out.status.success(),
        "a committed needle miss must still refuse the commit"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("occurs 0 time"),
        "the refusal must name the needle miss"
    );
    std::fs::remove_dir_all(&tmp).ok();
}

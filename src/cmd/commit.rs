//! `flywheel commit <paths>` — commit exactly the named paths (bd-aberm).
//!
//! A private `GIT_INDEX_FILE` is seeded from HEAD, so another lane's staged
//! entries in the shared index can never be swept into this commit. The repo's
//! own pre-commit hook fences the private tree before HEAD moves; HEAD advances
//! only by compare-and-swap, retried if another lane lands first.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};

const MAX_CAS_RETRIES: usize = 5;
static INDEX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One git invocation. With `index`, every git command sees only that private
/// index — the shared index stays untouched until the final path refresh.
struct Git<'a> {
    repo: &'a Path,
    index: Option<&'a Path>,
}

impl<'a> Git<'a> {
    fn shared(repo: &'a Path) -> Self {
        Self { repo, index: None }
    }

    fn private(repo: &'a Path, index: &'a Path) -> Self {
        Self {
            repo,
            index: Some(index),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.repo);
        if let Some(index) = self.index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.args(args);
        cmd
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        self.command(args)
            .output()
            .with_context(|| format!("git {args:?}"))
    }

    fn out(&self, args: &[&str]) -> Result<String> {
        let output = self.run(args)?;
        if !output.status.success() {
            bail!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    fn ok(&self, args: &[&str]) -> Result<()> {
        self.out(args).map(|_| ())
    }

    fn out_stdin(&self, args: &[&str], input: &[u8]) -> Result<String> {
        use std::io::Write as _;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("git {args:?}"))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("git {args:?} stdin"))?
            .write_all(input)
            .map_err(|e| anyhow!("git {args:?} stdin write: {e}"))?;
        let out = child
            .wait_with_output()
            .with_context(|| format!("git {args:?} wait"))?;
        if !out.status.success() {
            bail!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    }
}

struct IndexGuard(PathBuf);
impl Drop for IndexGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn validate_paths(paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        bail!("flywheel commit needs at least one path");
    }
    for path in paths {
        let p = Path::new(path);
        if p.as_os_str().is_empty() {
            bail!("empty path");
        }
        if p.is_absolute()
            || p.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("path must be repo-relative without '..': {path}");
        }
    }
    Ok(())
}

fn add_args<'a>(prefix: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut args = prefix.to_vec();
    for path in paths {
        args.push(path);
    }
    args
}

fn ensure_only_requested(staged: &str, paths: &[String]) -> Result<()> {
    for line in staged.lines().filter(|line| !line.is_empty()) {
        if !paths
            .iter()
            .any(|p| line == p || line.starts_with(format!("{p}/").as_str()))
        {
            bail!("private index staged an unrequested path: {line}");
        }
    }
    Ok(())
}

/// Run the repo's own commit fence against the private tree. A repo with no
/// pre-commit hook has no fence; every repo with one gets it, whatever language
/// its hook is written in.
fn fence_private_tree(repo: &Path, private: &Git<'_>, nonce: &str) -> Result<()> {
    let hook = private.out(&["rev-parse", "--git-path", "hooks/pre-commit"])?;
    let hook_path = if Path::new(&hook).is_absolute() {
        PathBuf::from(&hook)
    } else {
        repo.join(&hook)
    };
    if !hook_path.is_file() {
        return Ok(());
    }
    // The per-invocation nonce is what distinguishes a fenced `flywheel
    // commit` from a plain `git commit`; the installed hook refuses the
    // latter (bd-j1g64). Never forward an ambient token from the caller.
    let output = private
        .command(&["hook", "run", "pre-commit"])
        .env_remove("FLYWHEEL_COMMIT_OPERATOR")
        .env("FLYWHEEL_COMMIT_TOKEN", nonce)
        .output()
        .with_context(|| "git hook run pre-commit")?;
    if !output.status.success() {
        bail!(
            "commit fence failed, HEAD untouched: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Build the private tree on top of `parent`. Called again after a CAS race so
/// a tree built on the old HEAD can never revert what the other lane landed.
fn build_tree(private: &Git<'_>, paths: &[String], parent: &str) -> Result<String> {
    private.ok(&["read-tree", parent])?;
    private.ok(&add_args(&["add", "--"], paths))?;
    let staged = private.out(&["diff", "--cached", "--name-only"])?;
    ensure_only_requested(&staged, paths)?;
    private.out(&["write-tree"])
}

/// Build the private tree from exact blob contents: HEAD's tree, with each
/// named path replaced by the caller-supplied bytes (bd-kz3sc). The caller
/// decides which rows those bytes contain — this verb only stages them.
fn build_blobs_tree(
    private: &Git<'_>,
    blobs: &[(String, Vec<u8>)],
    parent: &str,
) -> Result<String> {
    private.ok(&["read-tree", parent])?;
    let paths: Vec<String> = blobs.iter().map(|(p, _)| p.clone()).collect();
    for (path, content) in blobs {
        let sha = private.out_stdin(&["hash-object", "-w", "--stdin"], content)?;
        private.ok(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{sha},{path}"),
        ])?;
    }
    let staged = private.out(&["diff", "--cached", "--name-only"])?;
    ensure_only_requested(&staged, &paths)?;
    private.out(&["write-tree"])
}

/// Never strip or restore a neighbour's hunks: refuse when a named path is
/// already staged in the shared index, naming it so the holder can be mailed.
fn refuse_foreign_stage(shared: &Git<'_>, paths: &[String]) -> Result<()> {
    let staged = shared.out(&["diff", "--cached", "--name-only"])?;
    let conflict: Vec<&str> = staged
        .lines()
        .filter(|line| {
            paths
                .iter()
                .any(|p| *line == p.as_str() || line.starts_with(format!("{p}/").as_str()))
        })
        .collect();
    if !conflict.is_empty() {
        bail!(
            "named path(s) already staged in the shared index (another lane's hunks?): {} — mail the holder, do not strip them",
            conflict.join(", ")
        );
    }
    Ok(())
}

/// Warn when the SHARED index holds a staged entry for a path outside this
/// commit whose blob came from an ancestor of HEAD, not from HEAD: a stale
/// stage that a later plain commit would revert (bd-j1g64).
fn stale_stage_warnings(shared: &Git<'_>, paths: &[String]) -> Result<Vec<String>> {
    let staged = shared.out(&["diff", "--cached", "--name-only"])?;
    let mut warnings = Vec::new();
    let ancestors = shared
        .out(&["rev-list", "--max-count=32", "HEAD^"])
        .unwrap_or_default(); // a one-commit repo has no ancestor to be stale from
    for line in staged.lines() {
        if paths
            .iter()
            .any(|p| line == p.as_str() || line.starts_with(format!("{p}/").as_str()))
        {
            continue; // already refused by refuse_foreign_stage
        }
        let Ok(staged_blob) = shared.out(&["rev-parse", &format!(":{line}")]) else {
            continue;
        };
        let Ok(head_blob) = shared.out(&["rev-parse", &format!("HEAD:{line}")]) else {
            continue;
        };
        if staged_blob == head_blob {
            continue;
        }
        for rev in ancestors.lines() {
            if let Ok(blob) = shared.out(&["rev-parse", &format!("{rev}:{line}")])
                && blob == staged_blob
            {
                warnings.push(format!(
                    "flywheel commit: WARNING: shared index has a stale stage for {line} \
                     (blob from {rev}, not HEAD) — a later plain commit would revert it; \
                     mail the holder"
                ));
                break;
            }
        }
    }
    Ok(warnings)
}

fn warn_stale_stage(shared: &Git<'_>, paths: &[String]) -> Result<()> {
    for warning in stale_stage_warnings(shared, paths)? {
        eprintln!("{warning}");
    }
    Ok(())
}

/// The one private-index fence + CAS path. `build` produces a tree on top of
/// the named parent; it is called again after a CAS race so a tree built on an
/// old HEAD can never revert what another lane landed.
fn commit_tree<F>(repo: &Path, paths: &[String], message: &str, mut build: F) -> Result<String>
where
    F: for<'a> FnMut(&Git<'a>, &str) -> Result<String>,
{
    let shared = Git::shared(repo);
    let old_head = shared.out(&["rev-parse", "HEAD"])?;
    refuse_foreign_stage(&shared, paths)?;
    warn_stale_stage(&shared, paths)?;

    let unique = format!(
        "{}-{}",
        std::process::id(),
        INDEX_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let nonce = format!("fwc-{unique}-{nanos}");
    let index = std::env::temp_dir().join(format!("fw-commit-{unique}"));
    let _guard = IndexGuard(index.clone());
    let private = Git::private(repo, &index);
    let mut tree = build(&private, &old_head)?;
    fence_private_tree(repo, &private, &nonce)?;

    let mut parent = old_head;
    for _ in 0..MAX_CAS_RETRIES {
        let commit = shared.out(&["commit-tree", &tree, "-p", &parent, "-m", message])?;
        let update = shared.run(&["update-ref", "HEAD", &commit, &parent])?;
        if update.status.success() {
            return Ok(commit);
        }
        let moved = shared.out(&["rev-parse", "HEAD"])?;
        if moved == parent {
            bail!(
                "update-ref failed without HEAD moving: {}",
                String::from_utf8_lossy(&update.stderr).trim()
            );
        }
        parent = moved;
        tree = build(&private, &parent)?;
        fence_private_tree(repo, &private, &nonce)?;
    }
    bail!("HEAD moved {MAX_CAS_RETRIES} times; refusing to commit")
}

/// Commit exactly `paths` from the working tree, returning the new commit sha.
pub(crate) fn commit_paths(repo: &Path, paths: &[String], message: &str) -> Result<String> {
    validate_paths(paths)?;
    let sha = commit_tree(repo, paths, message, |private, parent| {
        build_tree(private, paths, parent)
    })?;
    Git::shared(repo).ok(&add_args(&["add", "--"], paths))?;
    Ok(sha)
}

/// Commit exact `path -> content` blobs produced by `splice(parent)` (bd-kz3sc
/// rework 2). The splice is recomputed on EVERY CAS retry against the moved
/// parent, so a neighbour that landed in between is never reverted. The shared
/// index is refreshed from the final HEAD's splice.
pub(crate) fn commit_blobs<F>(
    repo: &Path,
    paths: &[String],
    message: &str,
    mut splice: F,
) -> Result<String>
where
    F: FnMut(&str) -> Result<Vec<(String, Vec<u8>)>>,
{
    validate_paths(paths)?;
    let sha = commit_tree(repo, paths, message, |private, parent| {
        let blobs = splice(parent)?;
        build_blobs_tree(private, &blobs, parent)
    })?;
    let shared = Git::shared(repo);
    let head = shared.out(&["rev-parse", "HEAD"])?;
    for (path, content) in splice(&head)? {
        let blob = shared.out_stdin(&["hash-object", "-w", "--stdin"], &content)?;
        shared.ok(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{path}"),
        ])?;
    }
    Ok(sha)
}

pub(crate) fn cmd_commit(paths: &[String], message: &str, repo: Option<&str>) -> Result<()> {
    let repo_arg = repo.unwrap_or(".");
    let repo = fs::canonicalize(repo_arg).with_context(|| format!("repo dir {repo_arg}"))?;
    let sha = commit_paths(&repo, paths, message)?;
    println!("commit {}", &sha[..sha.len().min(12)]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn hook(dir: &Path, body: &str) {
        let path = dir.join("hooks/pre-commit");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
    }

    fn scratch_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fw-commit-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("hooks")).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.name", "Test Agent"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "core.hooksPath", "hooks"]);
        fs::write(dir.join("base.txt"), "base\n").unwrap();
        git(&dir, &["add", "base.txt"]);
        git(&dir, &["commit", "-qm", "base"]);
        hook(&dir, "exit 0");
        dir
    }

    fn tracked_in_head(dir: &Path, sha: &str) -> Vec<String> {
        git(dir, &["ls-tree", "-r", "--name-only", sha])
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn shared_index_staged_files_are_never_swept() {
        let dir = scratch_repo("sweep");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        git(&dir, &["add", "theirs.txt"]);

        let sha = commit_paths(&dir, &["ours.txt".into()], "ours").unwrap();

        let files = tracked_in_head(&dir, &sha);
        assert!(files.contains(&"ours.txt".to_string()), "{files:?}");
        assert!(!files.contains(&"theirs.txt".to_string()), "{files:?}");
        let still_staged = git(&dir, &["diff", "--cached", "--name-only"]);
        assert!(
            still_staged.lines().any(|l| l == "theirs.txt"),
            "{still_staged}"
        );
        assert!(
            !still_staged.lines().any(|l| l == "ours.txt"),
            "{still_staged}"
        );
    }

    #[test]
    fn named_paths_are_committed_byte_equal() {
        let dir = scratch_repo("bytes");
        let body = "line one\nline two\n";
        fs::write(dir.join("ours.txt"), body).unwrap();
        let sha = commit_paths(&dir, &["ours.txt".into()], "bytes").unwrap();
        let committed = git(&dir, &["show", &format!("{sha}:ours.txt")]);
        assert_eq!(
            committed.replace('\r', ""),
            body.trim_end().replace('\r', "")
        );
    }

    #[test]
    fn fence_sees_only_the_private_tree() {
        let dir = scratch_repo("fence-tree");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        git(&dir, &["add", "theirs.txt"]);
        hook(&dir, "git diff --cached --name-only > fence-saw.txt");

        commit_paths(&dir, &["ours.txt".into()], "ok").unwrap();

        let saw = fs::read_to_string(dir.join("fence-saw.txt")).unwrap();
        assert!(saw.lines().any(|l| l == "ours.txt"), "{saw}");
        assert!(!saw.lines().any(|l| l == "theirs.txt"), "{saw}");
    }

    #[test]
    fn a_red_fence_leaves_head_and_shared_index_untouched() {
        let dir = scratch_repo("red");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        fs::write(dir.join("theirs.txt"), "theirs\n").unwrap();
        git(&dir, &["add", "theirs.txt"]);
        hook(&dir, "exit 1");
        let head = git(&dir, &["rev-parse", "HEAD"]);
        let staged = git(&dir, &["write-tree"]);

        let err = commit_paths(&dir, &["ours.txt".into()], "never").unwrap_err();
        assert!(format!("{err:#}").contains("fence failed"), "{err:#}");
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&dir, &["write-tree"]), staged);
    }

    #[test]
    fn concurrent_head_move_is_retried_not_overwritten() {
        let dir = scratch_repo("race");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        hook(
            &dir,
            "if [ ! -f race-moved ]; then \
               : > race-moved; \
               unset GIT_INDEX_FILE; \
               printf interloper > racer.txt; \
               git add racer.txt; \
               t=$(git write-tree); \
               o=$(git rev-parse HEAD); \
               n=$(printf race | git -c user.name=r -c user.email=r@x commit-tree \"$t\" -p \"$o\"); \
               git update-ref HEAD \"$n\" \"$o\"; \
             fi; exit 0",
        );
        let before = git(&dir, &["rev-parse", "HEAD"]);

        let sha = commit_paths(&dir, &["ours.txt".into()], "ours").unwrap();

        let parent = git(&dir, &["rev-parse", &format!("{sha}^")]);
        assert_ne!(
            parent, before,
            "the commit was re-parented onto the moved HEAD"
        );
        assert_eq!(git(&dir, &["log", "-1", "--format=%s", &parent]), "race");
        let files = tracked_in_head(&dir, &sha);
        assert!(files.contains(&"ours.txt".to_string()), "{files:?}");
        assert!(
            files.contains(&"racer.txt".to_string()),
            "the other lane's file was reverted by a stale tree: {files:?}"
        );
    }

    #[test]
    fn a_named_path_already_staged_is_refused_not_stripped() {
        let dir = scratch_repo("staged");
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        git(&dir, &["add", "ours.txt"]);

        let err = commit_paths(&dir, &["ours.txt".into()], "never").unwrap_err();
        assert!(
            format!("{err:#}").contains("ours.txt"),
            "the error must name the path: {err:#}"
        );
        assert!(
            git(&dir, &["diff", "--cached", "--name-only"])
                .lines()
                .any(|line| line == "ours.txt"),
            "the neighbour's staged hunk must be untouched"
        );
    }

    fn install_real_hook(dir: &Path) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let hook_body = fs::read_to_string(root.join("scripts/hooks/pre-commit")).unwrap();
        // The real hook execs the repo's fence; a stub keeps this unit test
        // about the nonce gate, not a full build.
        fs::create_dir_all(dir.join("scripts/hooks")).unwrap();
        let fence = dir.join("scripts/fence.sh");
        fs::write(&fence, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fence, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = dir.join("scripts/hooks/pre-commit");
        fs::write(&path, &hook_body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(dir, &["config", "core.hooksPath", "scripts/hooks"]);
    }

    #[test]
    fn a_stale_shared_stage_is_reported() {
        let dir = scratch_repo("stale-stage");
        fs::write(dir.join("base.txt"), "changed\n").unwrap();
        git(&dir, &["add", "base.txt"]);
        git(&dir, &["commit", "-qm", "second"]);
        let old = git(&dir, &["rev-parse", "HEAD^:base.txt"]);
        git(
            &dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{old},base.txt"),
            ],
        );
        let warnings = stale_stage_warnings(&Git::shared(&dir), &["ours.txt".into()]).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("base.txt") && w.contains("stale")),
            "the stale stage must be reported: {warnings:?}"
        );
    }

    #[test]
    fn a_plain_git_commit_is_refused_with_the_flywheel_commit_hint() {
        let dir = scratch_repo("hook-refuse");
        install_real_hook(&dir);
        fs::write(dir.join("plain.txt"), "x\n").unwrap();
        git(&dir, &["add", "plain.txt"]);
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["commit", "-qm", "plain"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "plain git commit must be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("flywheel commit"), "{err}");
    }

    #[test]
    fn a_token_or_the_operator_escape_allows_the_hook() {
        let dir = scratch_repo("hook-escape");
        install_real_hook(&dir);
        for (tag, env, value) in [
            ("token", "FLYWHEEL_COMMIT_TOKEN", "nonce-1"),
            ("operator", "FLYWHEEL_COMMIT_OPERATOR", "1"),
        ] {
            fs::write(dir.join(format!("{tag}.txt")), "x\n").unwrap();
            git(&dir, &["add", format!("{tag}.txt").as_str()]);
            let out = StdCommand::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["commit", "-qm", tag])
                .env(env, value)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{tag}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn flywheel_commit_supplies_the_nonce_to_the_installed_hook() {
        let dir = scratch_repo("hook-fwc");
        install_real_hook(&dir);
        fs::write(dir.join("ours.txt"), "ours\n").unwrap();
        let sha = commit_paths(&dir, &["ours.txt".into()], "ours").unwrap();
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]), sha);
    }

    #[test]
    fn absolute_and_parent_paths_are_refused() {
        let dir = scratch_repo("paths");
        assert!(commit_paths(&dir, &["/etc/passwd".into()], "no").is_err());
        assert!(commit_paths(&dir, &["../escape".into()], "no").is_err());
    }
}

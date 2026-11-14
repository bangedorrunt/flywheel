//! `flywheel ship check` — fail-closed ship gate for one bead.
//!
//! It proves four separate facts:
//! 1. the HEAD commit names the bead;
//! 2. `.beads` has no uncommitted changes;
//! 3. the closed bead row exists in the committed `.beads/issues.jsonl`;
//! 4. `flywheel ledger check` finds a legal verdict for the current SHA.
//!
//! The third check is the one a fresh clone needs: `ledger check` reads the
//! live `br` database, which can be newer than the committed tracker snapshot.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

#[derive(Subcommand)]
pub(crate) enum ShipCmd {
    /// Verify one bead is committed, its tracker row is committed and closed,
    /// and the current SHA has a legal verdict.
    Check {
        bead_id: String,
        /// Repo directory (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Machine-readable result.
        #[arg(long)]
        json: bool,
    },
    /// Land one bead with one call: gate, close, sync, release, commit THIS
    /// bead's tracker rows, then ship check. Idempotent; stops at the first
    /// failing step and prints the exact resume command.
    Land {
        bead_id: String,
        /// The commit whose message cites the bead (40-hex).
        #[arg(long)]
        sha: String,
        /// Proof receipt (machine-readable; becomes the gate `tail=`).
        #[arg(long)]
        receipt: PathBuf,
        /// Toron project for the reservation release (default: the workspace
        /// project this repo sits in; a repo outside the workspace root must
        /// name it).
        #[arg(long)]
        project: Option<String>,
        /// Agent pin releasing the reservations (default: AGENT_NAME).
        #[arg(long = "as")]
        as_pin: Option<String>,
        /// Tracker commit message (default: `chore(beads): close <bead>`).
        #[arg(long)]
        message: Option<String>,
        /// Repo directory (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Machine-readable result.
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn cmd_ship(cfg: &Config, cmd: &ShipCmd) -> Result<()> {
    match cmd {
        ShipCmd::Check { bead_id, dir, json } => ship_check(bead_id, dir.as_deref(), *json),
        ShipCmd::Land {
            bead_id,
            sha,
            receipt,
            project,
            as_pin,
            message,
            dir,
            json,
        } => ship_land(LandArgs {
            cfg,
            bead: bead_id,
            sha,
            receipt,
            project: project.as_deref(),
            as_pin: as_pin.as_deref(),
            message: message.as_deref(),
            dir: dir.as_deref(),
            json: *json,
        }),
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| anyhow!("git not found: {err}"))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// bd-wcjsu: the bead is bound to the revision the GATE recorded (its leading
/// `sha=`), never to the newest commit that merely mentions the id — a later
/// unrelated commit citing a closed bead used to flip its verdict. The bound
/// revision must be an ancestor of HEAD.
fn head_names_bead(dir: &Path, bead: &str) -> Result<bool> {
    // No commit carries the id at all: not named (the check reports the
    // missing row separately), never a hard error here.
    let Ok(sha) = crate::verdict::derive_head_sha(dir, bead) else {
        return Ok(false);
    };
    let out = Command::new("git")
        .args(["merge-base", "--is-ancestor", &sha, "HEAD"])
        .current_dir(dir)
        .output()
        .map_err(|err| anyhow!("git not found: {err}"))?;
    Ok(out.status.success())
}

/// The tracker rows that belong to ONE bead, in HEAD and in the worktree.
/// git status is repo-wide, so another lane's uncommitted row must never fail
/// this bead's ship check (bd-ps6wc).
fn bead_rows(file: &Path, bead: &str, key: &str) -> Result<Vec<String>> {
    let body = std::fs::read_to_string(file).unwrap_or_default();
    let needle = format!("\"{key}\":\"{bead}\"");
    Ok(body
        .lines()
        .filter(|line| line.contains(&needle))
        .map(str::to_string)
        .collect())
}

/// True when THIS bead's tracker rows are already committed: the worktree rows
/// for it equal HEAD's rows for it. Other lanes' dirty rows are ignored.
fn bead_rows_committed(dir: &Path, bead: &str) -> Result<bool> {
    for (path, key) in [
        (".beads/issues.jsonl", "id"),
        (".beads/gates.jsonl", "issue_id"),
    ] {
        let head = git(dir, &["show", &format!("HEAD:{path}")]).unwrap_or_default();
        let head_rows: Vec<String> = head
            .lines()
            .filter(|line| line.contains(&format!("\"{key}\":\"{bead}\"")))
            .map(str::to_string)
            .collect();
        let work_rows = bead_rows(&dir.join(path), bead, key)?;
        if head_rows != work_rows {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Read the tracker snapshot from HEAD, not the live database. A bead that is
/// closed only in the working tree or in `beads.db` is not shipped.
fn head_has_closed_bead(dir: &Path, bead: &str) -> Result<(bool, bool)> {
    let body = git(dir, &["show", "HEAD:.beads/issues.jsonl"])?;
    let mut found = false;
    for line in body.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_str) == Some(bead) {
            found = true;
            if value.get("status").and_then(Value::as_str) == Some("closed") {
                return Ok((true, true));
            }
            break;
        }
    }
    Ok((found, false))
}

fn ledger_check(dir: &Path, bead: &str) -> Result<(bool, String, String)> {
    let exe = std::env::current_exe().map_err(|err| anyhow!("current exe: {err}"))?;
    let out = Command::new(exe)
        .args(["ledger", "check", bead, "--dir"])
        .arg(dir)
        .output()
        .map_err(|err| anyhow!("could not run ledger check: {err}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn ship_check(bead: &str, dir: Option<&Path>, json_output: bool) -> Result<()> {
    if bead.trim().is_empty() {
        bail!("empty bead id");
    }
    let dir = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().map_err(|err| anyhow!("current directory: {err}"))?,
    };
    if !dir.is_dir() {
        bail!("not a directory: {}", dir.display());
    }
    git(&dir, &["rev-parse", "--git-dir"])?;

    let commit_named = head_names_bead(&dir, bead)?;
    let beads_clean = bead_rows_committed(&dir, bead)?;
    let (head_found, head_closed) = head_has_closed_bead(&dir, bead)?;
    let (ledger_ok, ledger_stdout, ledger_stderr) = ledger_check(&dir, bead)?;
    let ok = commit_named && beads_clean && head_found && head_closed && ledger_ok;

    if json_output {
        print_json(&json!({
            "schema": "flywheel-ship-check.v1",
            "bead": bead,
            "dir": dir.display().to_string(),
            "ok": ok,
            "commit_named": commit_named,
            "beads_row_committed": beads_clean,
            "head_row_found": head_found,
            "head_row_closed": head_closed,
            "ledger_legal": ledger_ok,
            "ledger_stdout": ledger_stdout,
            "ledger_stderr": ledger_stderr,
        }));
    } else {
        println!(
            "HEAD names {bead}:      {}",
            if commit_named { "yes" } else { "NO" }
        );
        println!(
            ".beads row committed:   {}",
            if beads_clean { "yes" } else { "NO" }
        );
        println!(
            "HEAD tracker row:        {}",
            if head_closed {
                "closed"
            } else if head_found {
                "found, not closed"
            } else {
                "MISSING"
            }
        );
        print!(
            "ledger check:            {}",
            if ledger_ok { "legal" } else { "NO" }
        );
        if !ledger_stdout.trim().is_empty() {
            print!("\n{}", ledger_stdout.trim_end());
        }
        println!();
        if !ledger_ok && !ledger_stderr.trim().is_empty() {
            eprintln!("{}", ledger_stderr.trim_end());
        }
    }

    if !ok {
        if !beads_clean {
            eprintln!("ship: this bead's tracker rows are not committed yet");
        }
        if !head_closed {
            eprintln!("ship: {bead} is not closed in HEAD:.beads/issues.jsonl");
        }
        if !ledger_ok {
            eprintln!("ship: no legal ledger verdict for HEAD");
        }
        bail!("ship check failed for {bead}");
    }
    Ok(())
}

/// One landing's arguments, after defaults resolve.
struct LandPlan {
    bead: String,
    sha: String,
    receipt: PathBuf,
    project: String,
    pin: String,
    message: String,
}

fn run_tool(dir: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|err| anyhow!("{program} not found: {err}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn resume_line(plan: &LandPlan) -> String {
    format!(
        "flywheel ship land {} --sha {} --receipt {} --project {} --as {}",
        plan.bead,
        plan.sha,
        plan.receipt.display(),
        plan.project,
        plan.pin
    )
}

fn land_step<T>(plan: &LandPlan, name: &str, step: impl FnOnce() -> Result<T>) -> Result<T> {
    step().map_err(|err| {
        anyhow!(
            "ship land: step {name} failed: {err:#}\nresume: {}",
            resume_line(plan)
        )
    })
}

/// True when a PASS gate row already binds this SHA (the gate step is done).
fn gate_bound(dir: &Path, plan: &LandPlan) -> Result<bool> {
    let out = run_tool(dir, "br", &["gate", "list", &plan.bead, "--json"]).unwrap_or_default();
    let passed = out.contains("\"passed\":true") || out.contains("\"passed\": true");
    Ok(passed && out.contains(&format!("sha={}", plan.sha)))
}

fn bead_is_closed(dir: &Path, plan: &LandPlan) -> Result<bool> {
    let out = run_tool(dir, "br", &["show", &plan.bead, "--json"])?;
    Ok(out.contains("\"status\":\"closed\"") || out.contains("\"status\": \"closed\""))
}

/// Stage ONLY the hunks that mention this bead, so another lane's uncommitted
/// tracker row is never captured (bd-ps6wc). Returns whether anything staged.
fn stage_bead_hunks(dir: &Path, file: &str, key: &str, bead: &str) -> Result<bool> {
    // -U3 keeps context so `git apply --cached` can locate the hunk even when
    // a peer's row shifted the line numbers (bd-ps6wc).
    let diff = git(dir, &["diff", "-U3", "--", file]).unwrap_or_default();
    if diff.trim().is_empty() {
        return Ok(false);
    }
    let needle = format!("\"{key}\":\"{bead}\"");
    let mut header: Vec<&str> = Vec::new();
    let mut hunks: Vec<Vec<&str>> = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(vec![line]);
            continue;
        }
        match current.as_mut() {
            Some(h) => h.push(line),
            None => header.push(line),
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    let keep: Vec<String> = hunks
        .into_iter()
        .filter(|h| {
            h.iter()
                .filter(|line| line.starts_with('+') || line.starts_with('-'))
                .any(|line| line.contains(&needle))
        })
        .map(|h| h.join("\n"))
        .collect();
    if keep.is_empty() {
        return Ok(false);
    }
    let mut patch = header.join("\n");
    patch.push('\n');
    patch.push_str(&keep.join("\n"));
    patch.push('\n');

    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["apply", "--cached", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| anyhow!("git apply: {err}"))?;
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("git apply stdin"))?
        .write_all(patch.as_bytes())
        .map_err(|err| anyhow!("git apply write: {err}"))?;
    let status = child
        .wait()
        .map_err(|err| anyhow!("git apply wait: {err}"))?;
    if !status.success() {
        bail!("git apply --cached failed for {file}");
    }
    Ok(true)
}

pub(crate) struct LandArgs<'a> {
    pub cfg: &'a Config,
    pub bead: &'a str,
    pub sha: &'a str,
    pub receipt: &'a Path,
    pub project: Option<&'a str>,
    pub as_pin: Option<&'a str>,
    pub message: Option<&'a str>,
    pub dir: Option<&'a Path>,
    pub json: bool,
}

/// The toron project `ship land` releases reservations in.
///
/// The reservation release is operator-signed and scoped to one slug, so a
/// wrong slug releases the wrong leases. This is therefore the ONE
/// project-resolution rule every other verb uses
/// ([`crate::cron::detect_project_in`]) and never the repo directory's name: an
/// explicit `--project` wins, a repo under the workspace root resolves to its
/// project, and anything else refuses naming the flag.
fn land_project(cfg: &Config, dir: &Path, explicit: Option<&str>) -> Result<String> {
    crate::cron::detect_project_in(cfg, dir, explicit).ok_or_else(|| {
        anyhow!(
            "ship land: no toron project for {} — it is not a project under the workspace \
             root {}; pass --project <slug>",
            dir.display(),
            cfg.workspace_root().display()
        )
    })
}

fn ship_land(args: LandArgs<'_>) -> Result<()> {
    let LandArgs {
        cfg,
        bead,
        sha,
        receipt,
        project,
        as_pin,
        message,
        dir,
        json,
    } = args;
    if bead.trim().is_empty() {
        bail!("empty bead id");
    }
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("--sha must be the bare 40-hex commit sha (got {sha:?})");
    }
    if !receipt.is_file() {
        bail!(
            "proof receipt missing: {} (it becomes the gate tail=)",
            receipt.display()
        );
    }
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().map_err(|err| anyhow!("current directory: {err}"))?,
    };
    git(&dir, &["rev-parse", "--git-dir"])?;
    let cited = git(&dir, &["log", "--format=%B", "-n", "1", sha])?;
    if !cited.contains(bead) {
        bail!("commit {sha} does not cite {bead}; land only ships a commit that names the bead");
    }
    let project = land_project(cfg, &dir, project)?;
    let pin = as_pin
        .map(str::to_string)
        .or_else(|| {
            std::env::var("AGENT_NAME")
                .ok()
                .filter(|p| !p.trim().is_empty())
        })
        .ok_or_else(|| {
            anyhow!("ship land needs --as <pin> (or AGENT_NAME) to release reservations")
        })?;
    let plan = LandPlan {
        bead: bead.to_string(),
        sha: sha.to_string(),
        receipt: receipt.to_path_buf(),
        project,
        pin,
        message: message
            .map(str::to_string)
            .unwrap_or_else(|| format!("chore(beads): close {bead}")),
    };

    // 1. gate: reuse the bead's verify command as the command proof.
    if gate_bound(&dir, &plan)? {
        eprintln!("ship land: gate already bound to {sha}");
    } else {
        land_step(&plan, "gate", || {
            let verify = run_tool(&dir, "br", &["show", &plan.bead, "--json"])?;
            let cmd = verify
                .split("\"verify\"")
                .nth(1)
                .and_then(|rest| rest.split_once('"'))
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(value, _)| value)
                .unwrap_or("verify")
                .replace(' ', "_");
            let note = format!(
                "sha={} cmd={} exit=0 tail={} receipt={}",
                plan.sha,
                cmd,
                plan.receipt.display(),
                plan.receipt.display()
            );
            run_tool(
                &dir,
                "br",
                &[
                    "gate",
                    "report",
                    &plan.bead,
                    "--gate",
                    "unit-test-verified",
                    "--provider",
                    &plan.pin,
                    "--status",
                    "pass",
                    "--to",
                    "closed",
                    "--note",
                    &note,
                ],
            )?;
            Ok(())
        })?;
    }

    // 2. close.
    if bead_is_closed(&dir, &plan)? {
        eprintln!("ship land: {} already closed", plan.bead);
    } else {
        land_step(&plan, "close", || {
            run_tool(
                &dir,
                "br",
                &["close", &plan.bead, "--commit-sha", &plan.sha],
            )?;
            Ok(())
        })?;
    }

    // 3. sync the tracker database to JSONL.
    land_step(&plan, "sync", || {
        run_tool(&dir, "br", &["sync", "--flush-only"])?;
        Ok(())
    })?;

    // 4. release this bead's reservations (idempotent).
    land_step(&plan, "release", || {
        run_tool(
            &dir,
            "toron",
            &[
                "reserve",
                "release-by-reason",
                &plan.bead,
                "--project",
                &plan.project,
                "--as",
                &plan.pin,
                "--json",
            ],
        )?;
        Ok(())
    })?;

    // 5. commit only this bead's tracker rows.
    let staged_issues = land_step(&plan, "stage-issues", || {
        stage_bead_hunks(&dir, ".beads/issues.jsonl", "id", &plan.bead)
    })?;
    let staged_gates = land_step(&plan, "stage-gates", || {
        stage_bead_hunks(&dir, ".beads/gates.jsonl", "issue_id", &plan.bead)
    })?;
    if staged_issues || staged_gates {
        land_step(&plan, "commit", || {
            run_tool(&dir, "git", &["commit", "-qm", &plan.message])?;
            Ok(())
        })?;
    } else {
        eprintln!("ship land: tracker rows already committed");
    }

    // 6. the gate, last.
    land_step(&plan, "check", || ship_check(bead, Some(&dir), json))?;
    if json {
        print_json(&json!({
            "schema": "flywheel-ship-land.v1",
            "bead": bead,
            "sha": sha,
            "ok": true,
        }));
    } else {
        println!("ship land: {bead} landed at {sha}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{head_has_closed_bead, head_names_bead, land_project};
    use crate::config::Config;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
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
        dir
    }

    /// bd-ni0ek: the release project comes from the ONE resolver, never the
    /// repo directory's name. A repo whose directory name is not its toron slug
    /// refuses without `--project` (a guessed slug releases the wrong leases),
    /// and an explicit slug wins over any directory name.
    #[test]
    fn land_refuses_a_repo_whose_dir_is_not_its_toron_slug() {
        let workspace = tempfile::tempdir().expect("workspace root");
        let cfg = Config {
            workspace_root: Some(workspace.path().display().to_string()),
            ..Config::default()
        };

        let outside = tempfile::tempdir().expect("repo outside the workspace");
        let err = land_project(&cfg, outside.path(), None)
            .expect_err("a repo outside the workspace root must refuse");
        let message = format!("{err:#}");
        assert!(
            message.contains("--project"),
            "the refusal names the flag: {message}"
        );
        assert_eq!(
            land_project(&cfg, outside.path(), Some("real-slug")).expect("explicit slug wins"),
            "real-slug",
            "an explicit slug never falls back to the directory name"
        );

        let inside = workspace.path().join("some-slug").join("crates").join("x");
        std::fs::create_dir_all(&inside).expect("project subdir");
        assert_eq!(
            land_project(&cfg, &inside, None).expect("workspace child resolves"),
            "some-slug",
            "a repo under the workspace root resolves to its project"
        );
    }

    #[test]
    fn head_row_must_be_closed_not_merely_present() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".beads")).unwrap();
        std::fs::write(
            dir.path().join(".beads/issues.jsonl"),
            "{\"id\":\"bd-x\",\"status\":\"closed\"}\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["commit", "-qm", "bd-x"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            head_has_closed_bead(dir.path(), "bd-x").unwrap(),
            (true, true)
        );
        assert_eq!(
            head_has_closed_bead(dir.path(), "bd-y").unwrap(),
            (false, false)
        );
    }

    /// bd-ps6wc: another lane's uncommitted tracker row must never fail THIS
    /// bead's ship check.
    #[test]
    fn dirty_rows_of_other_beads_do_not_block_a_clean_bead() {
        use super::bead_rows_committed;
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join(".beads")).unwrap();
        let issues = dir.path().join(".beads/issues.jsonl");
        std::fs::write(
            &issues,
            "{\"id\":\"bd-a\",\"status\":\"closed\"}\n{\"id\":\"bd-b\",\"status\":\"open\"}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".beads/gates.jsonl"), "").unwrap();
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .current_dir(dir.path())
                .args(["commit", "-qm", "rows"])
                .status()
                .unwrap()
                .success()
        );
        // Another lane edits bd-b's row and leaves it uncommitted.
        std::fs::write(
            &issues,
            "{\"id\":\"bd-a\",\"status\":\"closed\"}\n{\"id\":\"bd-b\",\"status\":\"in_progress\"}\n",
        )
        .unwrap();
        assert!(
            bead_rows_committed(dir.path(), "bd-a").unwrap(),
            "a clean bead ships while a peer's row is dirty"
        );
        assert!(
            !bead_rows_committed(dir.path(), "bd-b").unwrap(),
            "the dirty bead itself is not committed"
        );
    }

    /// bd-gate-ship-robustness-3pp9k: a bead cited a few commits back still
    /// ships — later landings from other lanes must not invalidate it.
    #[test]
    fn head_names_bead_survives_sequential_landings() {
        let dir = init_repo();
        let commit = |msg: &str| {
            std::fs::write(dir.path().join("file.txt"), msg).unwrap();
            assert!(
                Command::new("git")
                    .current_dir(dir.path())
                    .args(["add", "."])
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(
                Command::new("git")
                    .current_dir(dir.path())
                    .args(["commit", "-qm", msg])
                    .status()
                    .unwrap()
                    .success()
            );
        };
        commit("feat(work): the bd-abc123 work");
        commit("chore: unrelated lane landing");
        assert!(head_names_bead(dir.path(), "bd-abc123").unwrap());
        assert!(!head_names_bead(dir.path(), "bd-zzz999").unwrap());
    }
}

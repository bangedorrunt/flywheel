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
}

pub(crate) fn cmd_ship(_cfg: &Config, cmd: &ShipCmd) -> Result<()> {
    match cmd {
        ShipCmd::Check { bead_id, dir, json } => ship_check(bead_id, dir.as_deref(), *json),
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

fn head_names_bead(dir: &Path, bead: &str) -> Result<bool> {
    Ok(git(dir, &["log", "-1", "--format=%B"])?.contains(bead))
}

fn beads_worktree_clean(dir: &Path) -> Result<bool> {
    Ok(git(dir, &["status", "--porcelain", "--", ".beads"])?
        .trim()
        .is_empty())
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
    let beads_clean = beads_worktree_clean(&dir)?;
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
            "beads_clean": beads_clean,
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
            ".beads worktree clean:  {}",
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
            eprintln!("ship: commit the closed tracker snapshot before shipping");
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

#[cfg(test)]
mod tests {
    use super::head_has_closed_bead;
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
}

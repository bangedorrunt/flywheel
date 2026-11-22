//! `flywheel work` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::config::Config;
use crate::output::print_json;

#[derive(Subcommand)]
pub(crate) enum WorkCmd {
    /// bv --robot-triage: full graph triage with recommendations
    Triage,
    /// bv --robot-next: the single top pick
    Next,
    /// bv --robot-insights: graph metrics
    Insights,
    /// bv --robot-plan: parallel execution tracks
    Plan,
    /// br ready --json: ready beads
    Ready,
    /// bd-bead-create-contract-lint-4tdd: creation-time contract check —
    /// runs the EXACT loop parsers (verify fence + PRINCIPLES for P<=2)
    /// over the ready set and reports per-bead block reasons with the
    /// corrected snippet. Exit non-zero when any bead is not dispatchable.
    Check {
        /// project dir to run `br ready` in (default: current directory)
        project: Option<String>,
    },
    /// Close one bead-optional work unit (bd-ihiqo.3): run its verify, then
    /// close by the bead ceremony or, ad hoc, commit the named files plus
    /// a journal receipt. `--bead auto` mints a bead with `br q` when the
    /// verify fails, so untracked remainder is tracked instead of dropped.
    Close {
        /// Bead id to close (verify comes from `br show`).
        #[arg(long)]
        bead: Option<String>,
        /// Ad-hoc objective (requires --verify).
        #[arg(long)]
        objective: Option<String>,
        /// Verify command for ad-hoc work.
        #[arg(long)]
        verify: Option<String>,
        /// Gate name for the bead close (`br gate report` needs one; without
        /// it the command names the legal gates and stops).
        #[arg(long)]
        gate: Option<String>,
        /// Commit message for the ad-hoc close.
        #[arg(long)]
        message: Option<String>,
        /// Files to commit for an ad-hoc close (comma-separated globs).
        #[arg(long, value_delimiter = ',')]
        paths: Vec<String>,
        /// Mint a bead when the verify fails instead of dropping the remainder.
        #[arg(long)]
        auto_bead: bool,
        /// Repo to work in (default: current directory).
        #[arg(long)]
        dir: Option<String>,
    },
}

pub(crate) fn cmd_work(cfg: &Config, cmd: &WorkCmd) -> Result<()> {
    let cwd = crate::kernel::ProjectPath::parse(crate::authority::HostAuthority::resolve().cwd)
        .map_err(|error| anyhow::anyhow!("invalid project path: {error}"))?;
    let bv = crate::work::Bv::new(cwd.as_path().to_str().unwrap_or("."));
    match cmd {
        WorkCmd::Triage => {
            let v = bv.robot("triage")?;
            print_json(&v);
        }
        WorkCmd::Next => {
            let v = bv.robot("next")?;
            print_json(&v);
        }
        WorkCmd::Insights => {
            let v = bv.robot("insights")?;
            print_json(&v);
        }
        WorkCmd::Plan => {
            let v = bv.robot("plan")?;
            print_json(&v);
        }
        WorkCmd::Ready => {
            let v = bv.br_json(&["ready"])?;
            print_json(&v);
        }
        WorkCmd::Check { project } => cmd_work_check(cfg, project.as_deref())?,
        WorkCmd::Close {
            bead,
            objective,
            verify,
            gate,
            message,
            paths,
            auto_bead,
            dir,
        } => cmd_work_close(
            cfg,
            bead.as_deref(),
            objective.as_deref(),
            verify.as_deref(),
            gate.as_deref(),
            message.as_deref(),
            paths,
            *auto_bead,
            dir.as_deref(),
        )?,
    }
    let _ = cfg;
    Ok(())
}

/// `git` in one repo, loud on failure: a close must never cite a sha it
/// did not verify, and never close over an unreadable tree.
fn git(repo: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_head(repo: &std::path::Path) -> Result<String> {
    git(repo, &["rev-parse", "HEAD"])
}

fn git_message(repo: &std::path::Path, sha: &str) -> Result<String> {
    git(repo, &["log", "--format=%B", "-n", "1", sha])
}

/// Close one bead-optional work unit (bd-ihiqo.3): run its verify, then
/// close by the bead ceremony or, ad hoc, by commit plus journal receipt.
/// `--bead auto` mints a bead with `br q` when the verify fails, so the
/// remainder is tracked instead of dropped.
#[allow(clippy::too_many_arguments)]
fn cmd_work_close(
    _cfg: &Config,
    bead: Option<&str>,
    objective: Option<&str>,
    verify: Option<&str>,
    gate: Option<&str>,
    message: Option<&str>,
    paths: &[String],
    auto_bead: bool,
    dir: Option<&str>,
) -> Result<()> {
    use crate::work_unit::WorkRef;
    let repo = match dir {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::current_dir()?,
    };
    let target = match (bead, objective, verify) {
        (Some(id), _, _) if !id.trim().is_empty() => WorkRef::Bead { id: id.to_string() },
        (None, Some(objective), Some(verify)) if !objective.trim().is_empty() => WorkRef::AdHoc {
            objective: objective.to_string(),
            verify: verify.to_string(),
        },
        _ => anyhow::bail!("work close needs --bead <id> or both --objective and --verify"),
    };
    let verify_cmd = crate::work_unit::verify_command(&repo, &target)?;
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&verify_cmd)
        .current_dir(&repo)
        .output()?;
    let tail = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let tail: String = tail
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    if crate::work_unit::needs_bead_on_failure(out.status.success(), auto_bead)
        && matches!(target, WorkRef::AdHoc { .. })
    {
        let actor = std::env::var("AGENT_NAME")
            .ok()
            .filter(|a| !a.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("--auto-bead needs an agent identity: set AGENT_NAME or pass --as")
            })?;
        let objective = match &target {
            WorkRef::AdHoc { objective, .. } => objective.clone(),
            _ => unreachable!(),
        };
        let id = crate::br::Bead::create(
            Some(&repo),
            &[
                "q",
                "--actor",
                &actor,
                &format!("{objective} (verify failed: {verify_cmd})"),
            ],
        )?;
        print_json(&serde_json::json!({
            "closed": false,
            "bead": id.trim(),
            "next": "verify failed; remainder tracked — dress the bead (br update --verify/--principle) and re-run",
        }));
        anyhow::bail!("verify failed; remainder tracked");
    }
    if !out.status.success() {
        anyhow::bail!("verify failed:\n{tail}");
    }
    match target {
        WorkRef::Bead { id } => {
            let sha = git_head(&repo)?;
            let msg = git_message(&repo, &sha)?;
            if !msg.contains(&id) {
                anyhow::bail!(
                    "commit {sha} does not cite {id}; commit first with the bead id in the message"
                );
            }
            let gate = match gate {
                Some(g) if !g.trim().is_empty() => g.to_string(),
                _ => {
                    let dry = crate::br::run_br(
                        Some(&repo),
                        &["close", &id, "--dry-run", "--commit-sha", &sha],
                    )
                    .unwrap_or_default();
                    anyhow::bail!(
                        "bead close needs --gate <name>; legal gates per dry run:\n{dry}"
                    );
                }
            };
            let actor = std::env::var("AGENT_NAME").unwrap_or_else(|_| "assistant".into());
            crate::br::run_br(
                Some(&repo),
                &[
                    "gate",
                    "report",
                    &id,
                    "--gate",
                    &gate,
                    "--provider",
                    &actor,
                    "--status",
                    "pass",
                    "--to",
                    "closed",
                    "--note",
                    &format!("task={id} verdict={gate} sha={sha} closed by work close"),
                ],
            )?;
            let closed = crate::br::run_br(
                Some(&repo),
                &[
                    "close",
                    &id,
                    "--task-id",
                    &id,
                    "--verdict-id",
                    &gate,
                    "--reason",
                    &format!("closed by work close; verify green: {verify_cmd}"),
                ],
            )?;
            print_json(&serde_json::json!({
                "closed": true,
                "bead": id,
                "sha": sha,
                "detail": closed.lines().next().unwrap_or("").trim(),
            }));
        }
        WorkRef::AdHoc { objective, .. } => {
            if paths.is_empty() {
                anyhow::bail!("ad-hoc close needs --paths <globs> to commit");
            }
            let message = message
                .filter(|m| !m.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("chore: {objective}"));
            let ctx = crate::cmd::commit::ReservationCtx::from_env();
            let sha = crate::cmd::commit::commit_paths_checked(
                &repo, paths, &message, false, false, false, &ctx,
            )?;
            let receipt = crate::work_unit::adhoc_receipt_path(&repo, &objective, &sha);
            if let Some(parent) = receipt.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &receipt,
                crate::work_unit::close_report("closed", &sha, &verify_cmd, &tail),
            )?;
            print_json(&serde_json::json!({
                "closed": true,
                "sha": sha,
                "receipt": receipt.display().to_string(),
            }));
        }
    }
    Ok(())
}

fn cmd_work_check(cfg: &Config, project: Option<&str>) -> Result<()> {
    let dir = match project {
        Some(p) => crate::project_dir(cfg, p),
        None => std::env::current_dir()?,
    };
    let ready = crate::br::Bead::ready(Some(&dir))?;
    let mut issues: Vec<crate::durable::BeadContractIssue> = Vec::new();
    for b in &ready {
        let desc = bead_description(&dir, &b.id);
        if let Some(issue) = crate::durable::contract_issue(b, &desc) {
            issues.push(issue);
        }
    }
    if issues.is_empty() {
        println!("work check: {} ready beads, all dispatchable", ready.len());
        return Ok(());
    }
    for i in &issues {
        println!("BLOCK {} — {}", i.id, i.reason);
        println!("      fix: {}", i.fix);
    }
    bail!(
        "work check: {} of {} ready beads not dispatchable",
        issues.len(),
        ready.len()
    );
}

/// The raw description of a bead via `br show <id> --json` (empty on any
/// failure — a missing description only yields the fence/PRINCIPLES
/// verdicts the ready JSON already expresses).
fn bead_description(dir: &std::path::Path, id: &str) -> String {
    let Ok(raw) = crate::br::run_br(Some(dir), &["show", id, "--json"]) else {
        return String::new();
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| {
            v.as_array()
                .and_then(|a| a.first())
                .and_then(|b| b.get("description"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

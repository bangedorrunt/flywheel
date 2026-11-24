//! `flywheel blocked-by` (bd-6xu6r): parse a failed build log, attribute the
//! first compiler error to a path, find that path's holder, and send ONE
//! deduplicated alert with the error snippet. Never alerts the caller about
//! the caller's own file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use serde_json::Value;
use toron_core::kernel::MachineState;

use crate::cmd::build_break_statechart::{
    BuildBreakCmd, BuildBreakEvent, BuildBreakState, build_build_break_machine,
};
use crate::config::Config;

fn journal_path() -> PathBuf {
    crate::config::state_dir().join("build-break-alerts.json")
}

fn load_journal() -> BTreeSet<String> {
    std::fs::read_to_string(journal_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn save_journal(keys: &BTreeSet<String>) -> Result<()> {
    let path = journal_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(&keys.iter().collect::<Vec<_>>())?,
    )?;
    Ok(())
}

/// The first `--> path:line:col` after an `error` line, with a short snippet.
pub(crate) fn parse_build_error(log: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = log.lines().collect();
    let mut error_at = None;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("error") || line.trim_start().starts_with("error[") {
            error_at = Some(i);
            break;
        }
    }
    let start = error_at?;
    let snippet = lines[start..lines.len().min(start + 4)].join("\n");
    for line in lines.iter().skip(start).take(8) {
        let Some(rest) = line.trim().strip_prefix("--> ") else {
            continue;
        };
        let file = rest.split(':').next().unwrap_or(rest).trim();
        if !file.is_empty() {
            return Some((file.to_string(), snippet));
        }
    }
    None
}

fn holder_from_reservations(project: &str, file: &str) -> Option<String> {
    let out = Command::new("toron")
        .args(["reserve", "list", "--project", project, "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let rows = v.get("reservations")?.as_array()?;
    rows.iter().find_map(|r| {
        let path = r.get("path")?.as_str()?;
        let holder = r.get("holder")?.as_str()?;
        if path == file || path.ends_with(file) || file.ends_with(path) {
            Some(holder.to_string())
        } else {
            None
        }
    })
}

fn holder_from_git(file: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["log", "-1", "--format=%an", "--", file])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

pub(crate) fn cmd_blocked_by(
    _cfg: &Config,
    log: Option<&str>,
    caller: &str,
    project: &str,
    json: bool,
    dry_run: bool,
) -> Result<()> {
    let text = match log {
        Some(path) => std::fs::read_to_string(Path::new(path))?,
        None => {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
    };
    let (file, snippet) = parse_build_error(&text)
        .ok_or_else(|| anyhow::anyhow!("no compiler error with a --> path in the build log"))?;
    let holder = holder_from_reservations(project, &file)
        .or_else(|| holder_from_git(&file))
        .unwrap_or_default();
    let key = format!("{project}:{file}");

    let machine = build_build_break_machine();
    let mut ms = MachineState::at(BuildBreakState::Idle, Default::default());
    let out = machine.step_state(
        &ms,
        &BuildBreakEvent::Broken {
            key: key.clone(),
            file: file.clone(),
            snippet: snippet.clone(),
            holder: holder.clone(),
            caller: caller.to_string(),
        },
    );
    ms = MachineState::at(out.configuration.primary().cloned().unwrap(), out.context);
    let mails: Vec<_> = out
        .commands
        .iter()
        .filter_map(|c| match c {
            BuildBreakCmd::Mail { owner, body } => Some((owner.clone(), body.clone())),
            _ => None,
        })
        .collect();

    let mut journal = load_journal();
    let mut sent = false;
    for (owner, body) in mails {
        if journal.contains(&key) {
            continue;
        }
        // Journal before the provider runs: a replay cannot send twice.
        journal.insert(key.clone());
        save_journal(&journal)?;
        if dry_run {
            println!("blocked-by: would mail {owner}: {body}");
        } else {
            let status = Command::new("toron")
                .args([
                    "mail",
                    "send",
                    "--project",
                    project,
                    "--as",
                    caller,
                    "--to",
                    &owner,
                    "--subject",
                    &format!("[build-break] {file}"),
                    "--body",
                    &body,
                ])
                .status()?;
            if !status.success() {
                bail!("failed to mail build-break holder {owner}");
            }
        }
        sent = true;
    }
    let _ = ms;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "kind": "flywheel-blocked-by.v1",
                "file": file,
                "holder": holder,
                "alerted": sent,
                "key": key,
            })
        );
    } else if !sent {
        println!("blocked-by: no alert needed for {file}");
    }
    Ok(())
}

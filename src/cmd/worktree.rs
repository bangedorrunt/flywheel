//! `flywheel worktree` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::Result;
use clap::Subcommand;
use serde_json::Value;

use crate::config::Config;
use crate::herdr::Client;
use crate::output::{dim, kind_paint};

#[derive(Subcommand)]
pub(crate) enum WorktreeCmd {
    List {
        project: String,
    },
    Create {
        project: String,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },
    Open {
        project: String,
        #[arg(long)]
        branch: String,
    },
    Remove {
        project: String,
        #[arg(long)]
        force: bool,
    },
}

pub(crate) fn cmd_worktree(cfg: &Config, cmd: &WorktreeCmd) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    match cmd {
        WorktreeCmd::List { project } => {
            let ws_id = crate::find_workspace(&mut c, project)?;
            let v = crate::herdr::worktree_list(&mut c, Some(&ws_id))?;
            for w in &v {
                println!(
                    "{}  {}  {}",
                    kind_paint(&w.branch.clone().unwrap_or_default()),
                    w.open_workspace_id.clone().unwrap_or_default(),
                    dim(&w.path)
                );
            }
            if v.is_empty() {
                println!("no worktrees for {project}");
            }
        }
        WorktreeCmd::Create {
            project,
            branch,
            base,
            label,
        } => {
            let ws_id = crate::find_workspace(&mut c, project)?;
            let v = c.worktree_create(&ws_id, branch, base.as_deref(), label.as_deref(), false)?;
            println!(
                "worktree {} created: {}",
                branch,
                v.get("workspace")
                    .and_then(|w| w.get("workspace_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            );
        }
        WorktreeCmd::Open { project, branch } => {
            let ws_id = crate::find_workspace(&mut c, project)?;
            let v = c.worktree_create(&ws_id, branch, None, None, true)?;
            println!(
                "worktree {} opened: {}",
                branch,
                v.get("workspace")
                    .and_then(|w| w.get("workspace_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            );
        }
        WorktreeCmd::Remove { project, force } => {
            let ws_id = crate::find_workspace(&mut c, project)?;
            c.worktree_remove(&ws_id, *force)?;
            println!("worktree removed for {project}");
        }
    }
    Ok(())
}

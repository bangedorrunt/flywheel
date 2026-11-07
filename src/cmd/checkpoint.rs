//! `flywheel checkpoint` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::config::Config;
use crate::output::{dim, kind_paint, print_json, status_paint};

#[derive(Subcommand)]
pub(crate) enum CheckpointCmd {
    Save {
        project: String,
        #[arg(long)]
        msg: String,
    },
    List {
        project: String,
    },
    Show {
        project: String,
    },
    Latest {
        project: String,
    },
}

pub(crate) fn cmd_checkpoint(cfg: &Config, cmd: &CheckpointCmd) -> Result<()> {
    match cmd {
        CheckpointCmd::Save { project, msg } => {
            let mux = crate::backend::multiplexer_backend(cfg)?;
            let ws_id = crate::find_workspace_dyn(&*mux, project)?;
            let agents = mux
                .agent_list()?
                .into_iter()
                .filter(|a| a.workspace_id == ws_id)
                .collect::<Vec<_>>();
            let cp_agents: Vec<crate::state::CheckpointAgent> = agents
                .iter()
                .map(|a| crate::state::CheckpointAgent {
                    name: a.name.clone().unwrap_or_default(),
                    kind: a.agent.clone().unwrap_or_default(),
                    status: a.agent_status,
                    pane_id: a.pane_id.clone(),
                })
                .collect();
            let path = crate::state::save_checkpoint(
                project,
                msg,
                Some(&ws_id),
                cp_agents,
                Some(&crate::project_dir_str(cfg, project)?),
            )?;
            crate::log(
                project,
                crate::state::EventKind::Checkpoint,
                serde_json::json!({ "message": msg, "path": path.display().to_string() }),
            );
            println!("checkpoint saved: {}", dim(&path.display().to_string()));
        }
        CheckpointCmd::List { project } => {
            super::history::cmd_history(project)?;
        }
        CheckpointCmd::Show { project } => {
            if let Some(cp) = crate::state::latest_checkpoint(project)? {
                print_json(&serde_json::to_value(&cp)?);
            } else {
                bail!("no checkpoints for {project}");
            }
        }
        CheckpointCmd::Latest { project } => {
            if let Some(cp) = crate::state::latest_checkpoint(project)? {
                println!("{} {}  {}", dim(&cp.ts), dim(&cp.project), cp.message);
                for a in &cp.agents {
                    println!("  {}  {}", kind_paint(&a.kind), status_paint(a.status));
                }
            } else {
                bail!("no checkpoints for {project}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod plane_tests {
    /// A checkpoint is project state, not hc state: a rex crew must snapshot.
    /// The door builds its backend from config, so assert the seam at source.
    #[test]
    fn checkpoint_save_resolves_the_active_plane() {
        let src = include_str!("checkpoint.rs");
        let resolved = concat!("multiplexer_backend", "(cfg)");
        let hard_dial = concat!("Client", "::connect");
        assert!(
            src.contains(resolved),
            "checkpoint must build the active-plane backend"
        );
        assert!(
            !src.contains(hard_dial),
            "checkpoint must not hard-dial the hc socket"
        );
    }
}

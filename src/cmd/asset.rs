//! `flywheel asset` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::config::Config;

#[derive(Subcommand)]
pub(crate) enum AssetCmd {
    List,
    Run { name: String, project: String },
}

pub(crate) fn cmd_asset(cfg: &Config, kind: &str, cmd: &AssetCmd) -> Result<()> {
    match cmd {
        AssetCmd::List => {
            let names: Vec<String> = match kind {
                "recipes" | "template" => crate::config::Recipes::load()?
                    .recipes
                    .keys()
                    .cloned()
                    .collect(),
                "workflows" => crate::config::Workflows::load()?
                    .workflows
                    .keys()
                    .cloned()
                    .collect(),
                "pipeline" => crate::config::Pipelines::load()?
                    .pipelines
                    .keys()
                    .cloned()
                    .collect(),
                _ => vec![],
            };
            if names.is_empty() {
                println!(
                    "no {kind} defined (edit {})",
                    crate::config::config_dir()
                        .join(format!("{kind}.toml"))
                        .display()
                );
            } else {
                for n in names {
                    println!("{n}");
                }
            }
            Ok(())
        }
        AssetCmd::Run { name, project } => match kind {
            "recipes" | "template" => crate::cmd_spawn(
                cfg,
                project,
                crate::SpawnOpts {
                    recipe: Some(name.clone()),
                    ..Default::default()
                },
            ),
            "workflows" => crate::run_workflow(cfg, project, name),
            "pipeline" => crate::run_pipeline(cfg, project, name),
            _ => bail!("unknown asset kind {kind}"),
        },
    }
}

//! `flywheel policy` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::config::{Config, Policy};

#[derive(Subcommand)]
pub(crate) enum PolicyCmd {
    List,
    Add { pattern: String },
    Remove { index: usize },
    Path,
}

pub(crate) fn cmd_policy(_cfg: &Config, cmd: &PolicyCmd) -> Result<()> {
    let path = crate::config::config_dir().join("policy.toml");
    let mut policy = Policy::load()?;
    match cmd {
        PolicyCmd::List => {
            for (i, p) in policy.deny.iter().enumerate() {
                println!("[{i}] {p}");
            }
            if policy.deny.is_empty() {
                println!("no deny patterns");
            }
        }
        PolicyCmd::Add { pattern } => {
            policy.deny.push(pattern.clone());
            save_policy(&path, &policy)?;
            println!("guard added: {pattern}");
        }
        PolicyCmd::Remove { index } => {
            if *index >= policy.deny.len() {
                bail!("no guard at index {index}");
            }
            let removed = policy.deny.remove(*index);
            save_policy(&path, &policy)?;
            println!("guard removed: {removed}");
        }
        PolicyCmd::Path => {
            println!("{}", path.display());
        }
    }
    Ok(())
}

fn save_policy(path: &std::path::Path, policy: &Policy) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = toml::to_string_pretty(policy)?;
    std::fs::write(path, raw)?;
    Ok(())
}

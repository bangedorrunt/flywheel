//! `flywheel config` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::config::Config;

#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
    List,
    Get { key: String },
    Set { key: String, value: String },
    Path,
}

pub(crate) fn cmd_config(cfg: &Config, cmd: &ConfigCmd) -> Result<()> {
    let path = crate::config::config_file();
    match cmd {
        ConfigCmd::List => {
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            println!("{raw}");
            println!("# effective (flags/env > file):");
            println!("socket_path = {:?}", cfg.socket_path());
            println!(
                "workspace_root = {:?}",
                cfg.workspace_root().display().to_string()
            );
            println!("default_timeout_ms = {}", cfg.timeout_ms());
        }
        ConfigCmd::Get { key } => {
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            let cfg: crate::config::Config = toml::from_str(&raw).unwrap_or_default();
            let value = match key.as_str() {
                "socket_path" => cfg.socket_path,
                "workspace_root" => cfg.workspace_root,
                "default_timeout_ms" => cfg.default_timeout_ms.map(|v| v.to_string()),
                _ => bail!("unknown key: {key} (socket_path|workspace_root|default_timeout_ms)"),
            };
            println!("{}", value.unwrap_or_default());
        }
        ConfigCmd::Set { key, value } => {
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            let mut cfg: crate::config::Config = toml::from_str(&raw).unwrap_or_default();
            match key.as_str() {
                "socket_path" => cfg.socket_path = Some(value.clone()),
                "workspace_root" => cfg.workspace_root = Some(value.clone()),
                "default_timeout_ms" => cfg.default_timeout_ms = Some(value.parse()?),
                "backend.multiplexer" => {
                    cfg.backend.get_or_insert_with(Default::default).multiplexer = value.clone();
                }
                "backend.mail" => {
                    cfg.backend.get_or_insert_with(Default::default).mail = value.clone();
                }
                _ => bail!(
                    "unknown key: {key} (socket_path|workspace_root|default_timeout_ms|backend.multiplexer|backend.mail)"
                ),
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
            println!("set {key} = {value}");
        }
        ConfigCmd::Path => println!("{}", path.display()),
    }
    Ok(())
}

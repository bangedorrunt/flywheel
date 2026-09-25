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

/// Every key `flywheel config get/set` understands. Kept in one place so Get
/// and Set can never disagree about what is configurable (the bug: `Config`
/// read `resonate_url` but the CLI allowlist omitted it).
pub(crate) const CONFIG_KEYS: &[&str] = &[
    "socket_path",
    "workspace_root",
    "default_timeout_ms",
    "resonate_url",
    "backend.multiplexer",
    "backend.mail",
];

/// Apply one `config set` key/value onto a parsed [`Config`]. Pure — the sole
/// authority mapping a CLI key to a config field. Returns an error for an
/// unknown key so Get and Set share one allowlist.
pub(crate) fn apply_key(cfg: &mut Config, key: &str, value: &str) -> Result<()> {
    match key {
        "socket_path" => cfg.socket_path = Some(value.to_string()),
        "workspace_root" => cfg.workspace_root = Some(value.to_string()),
        "default_timeout_ms" => cfg.default_timeout_ms = Some(value.parse()?),
        "resonate_url" => cfg.resonate_url = Some(value.to_string()),
        "backend.multiplexer" => {
            cfg.backend.get_or_insert_with(Default::default).multiplexer = value.to_string();
        }
        "backend.mail" => {
            cfg.backend.get_or_insert_with(Default::default).mail = value.to_string();
        }
        _ => bail!("unknown key: {key} ({})", CONFIG_KEYS.join("|")),
    }
    Ok(())
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
                "resonate_url" => cfg.resonate_url,
                _ => bail!("unknown key: {key} ({})", CONFIG_KEYS.join("|")),
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
            apply_key(&mut cfg, key, value)?;
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

#[cfg(test)]
mod config_key_tests {
    use super::{CONFIG_KEYS, apply_key};
    use crate::config::Config;

    /// The CLI must be able to set every field `Config` actually reads —
    /// `resonate_url` is the regression: it was readable but not settable, so
    /// the documented "edit your config" path silently ignored it.
    #[test]
    fn apply_key_sets_resonate_url_and_round_trips() {
        let mut cfg = Config::default();
        apply_key(&mut cfg, "resonate_url", "https://resonate.example.ts.net").unwrap();
        assert_eq!(
            cfg.resonate_url.as_deref(),
            Some("https://resonate.example.ts.net")
        );
    }

    /// An unknown key is refused loudly and changes nothing (never a silent
    /// default write).
    #[test]
    fn apply_key_rejects_unknown_key() {
        let mut cfg = Config::default();
        let before = cfg.clone();
        let err = apply_key(&mut cfg, "not_a_key", "x").unwrap_err();
        assert!(err.to_string().contains("unknown key"));
        assert_eq!(cfg.resonate_url, before.resonate_url);
    }

    /// Every advertised key is accepted by the shared mapper (Get/Set parity).
    #[test]
    fn every_advertised_key_is_settable() {
        for key in CONFIG_KEYS {
            let mut cfg = Config::default();
            // default_timeout_ms parses; the rest take arbitrary strings.
            let v = if *key == "default_timeout_ms" {
                "5"
            } else {
                "x"
            };
            apply_key(&mut cfg, key, v)
                .unwrap_or_else(|e| panic!("advertised key {key} rejected: {e}"));
        }
    }
}

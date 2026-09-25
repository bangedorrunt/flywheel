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

/// Apply one `config set` key/value onto a parsed [`toml::Table`]. Mutates the
/// table IN PLACE and touches only the named key, so any field the running
/// binary does not model (a newer binary's setting, or a field this version
/// dropped) is preserved byte-for-byte on rewrite. This is why Set edits the
/// raw table instead of round-tripping through [`Config`]: deserializing into
/// `Config` and re-serializing DROPS every unrecognized field, so any config
/// write from a stale binary silently erased newer keys (observed: a
/// `resonate_url` set by hand vanished after an `install --force` + `set`
/// rewrite). Pure — the sole authority mapping a CLI key to a config path.
/// Returns an error for an unknown key so Get and Set share one allowlist.
pub(crate) fn apply_key(table: &mut toml::Table, key: &str, value: &str) -> Result<()> {
    // default_timeout_ms is the only non-string scalar; parse it or fail loud.
    let parsed: toml::Value = if key == "default_timeout_ms" {
        toml::Value::Integer(value.parse::<i64>().map_err(|e| {
            anyhow::anyhow!("{key} must be an integer number of ms, got {value:?}: {e}")
        })?)
    } else {
        toml::Value::String(value.to_string())
    };
    match key {
        "socket_path" | "workspace_root" | "default_timeout_ms" | "resonate_url" => {
            table.insert(key.to_string(), parsed);
        }
        "backend.multiplexer" | "backend.mail" => {
            let leaf = key.strip_prefix("backend.").expect("prefixed key");
            let backend = table
                .entry("backend".to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(t) = backend {
                t.insert(leaf.to_string(), parsed);
            } else {
                bail!("{key}: config `backend` is not a table — refusing to overwrite it");
            }
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
            // Edit the parsed table, not a `Config` round-trip: preserves
            // every field this binary does not model (see apply_key).
            let mut table: toml::Table = toml::from_str(&raw).unwrap_or_default();
            apply_key(&mut table, key, value)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, toml::to_string_pretty(&table)?)?;
            println!("set {key} = {value}");
        }
        ConfigCmd::Path => println!("{}", path.display()),
    }
    Ok(())
}

#[cfg(test)]
mod config_key_tests {
    use super::{CONFIG_KEYS, apply_key};

    /// The CLI must be able to set every field `Config` actually reads —
    /// `resonate_url` is the regression: it was readable but not settable, so
    /// the documented "edit your config" path silently ignored it.
    #[test]
    fn apply_key_sets_resonate_url_and_round_trips() {
        let mut table = toml::Table::new();
        apply_key(
            &mut table,
            "resonate_url",
            "https://resonate.example.ts.net",
        )
        .unwrap();
        let reloaded: toml::Table =
            toml::from_str(&toml::to_string_pretty(&table).unwrap()).unwrap();
        assert_eq!(
            reloaded.get("resonate_url").and_then(toml::Value::as_str),
            Some("https://resonate.example.ts.net")
        );
    }

    /// THE forward-compat regression: a `config set` must not erase any field
    /// the running binary does not model. A stale `install`/`set` round-trip
    /// through `Config` used to drop unknown fields (a hand-set
    /// `resonate_url` vanished). Setting one key must leave a foreign key and
    /// a nested table byte-intact.
    #[test]
    fn apply_key_preserves_fields_this_binary_does_not_model() {
        let mut table: toml::Table = toml::from_str(
            r#"
            resonate_url = "https://resonate.example.ts.net"
            future_key_from_a_newer_binary = "keep-me"
            [some_future_section]
            nested = 42
            "#,
        )
        .unwrap();
        apply_key(&mut table, "socket_path", "/tmp/fw.sock").unwrap();
        let out = toml::to_string_pretty(&table).unwrap();
        assert!(out.contains("future_key_from_a_newer_binary"), "{out}");
        assert!(out.contains("keep-me"), "{out}");
        assert!(out.contains("some_future_section"), "{out}");
        assert!(out.contains("nested = 42"), "{out}");
        assert!(
            out.contains("resonate_url"),
            "must not drop the set field: {out}"
        );
    }

    /// An unknown key is refused loudly and changes nothing (never a silent
    /// default write).
    #[test]
    fn apply_key_rejects_unknown_key() {
        let mut table: toml::Table =
            toml::from_str("resonate_url = \"https://keep.example\"").unwrap();
        let err = apply_key(&mut table, "not_a_key", "x").unwrap_err();
        assert!(err.to_string().contains("unknown key"));
        assert_eq!(
            table.get("resonate_url").and_then(toml::Value::as_str),
            Some("https://keep.example"),
            "a rejected set must not touch the table"
        );
    }

    /// Every advertised key is accepted by the shared mapper (Get/Set parity).
    #[test]
    fn every_advertised_key_is_settable() {
        for key in CONFIG_KEYS {
            let mut table = toml::Table::new();
            // default_timeout_ms parses as an integer; the rest take strings.
            let v = if *key == "default_timeout_ms" {
                "5"
            } else {
                "x"
            };
            apply_key(&mut table, key, v)
                .unwrap_or_else(|e| panic!("advertised key {key} rejected: {e}"));
        }
    }

    /// A non-integer `default_timeout_ms` fails loud (no silent fallback to a
    /// default — a typo must never be written as a wrong number).
    #[test]
    fn default_timeout_ms_rejects_non_integer() {
        let mut table = toml::Table::new();
        let err = apply_key(&mut table, "default_timeout_ms", "soon").unwrap_err();
        assert!(err.to_string().contains("integer"), "{err}");
    }
}

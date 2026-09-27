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
    /// Fill ~/.config/flywheel from compiled-in defaults: workflows, stages,
    /// and judgment packs.
    ///
    /// Default `--overwrite none` creates missing files and leaves every
    /// existing file alone, including one drift calls stale. `stale` replaces
    /// only shipped names whose bytes differ. `all` restores every shipped
    /// name. A file the binary does not ship is never touched. Every replaced
    /// file is copied to `<name>.bak` first. `config.toml` is created once
    /// from the commented template and is never overwritten.
    Seed {
        /// JSON report
        #[arg(long)]
        json: bool,
        /// none (fill gaps, default) | stale (repair drifted shipped copies) | all (restore every shipped copy)
        #[arg(long, value_name = "SCOPE", value_parser = ["none", "stale", "all"], default_value = "none")]
        overwrite: String,
        /// Print the plan and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Accept a destructive overwrite with no prompt. Required when there is no TTY.
        #[arg(long)]
        yes: bool,
    },
    /// Report how the user config differs from compiled-in defaults.
    /// Reads workflows, stages, judgment packs, and whether config.toml exists.
    /// Does not write.
    Drift {
        /// JSON rows
        #[arg(long)]
        json: bool,
    },
}

/// One operator setting: the key `config get/set` accepts, what it does, and
/// the default when the key is absent.
pub(crate) struct Setting {
    pub key: &'static str,
    pub summary: &'static str,
    pub default_hint: &'static str,
}

/// Every setting this binary documents. `config list` prints these. `config
/// seed` writes them as comments into a new config.toml. Get and Set refuse
/// a key that is not in this list.
pub(crate) const SETTINGS: &[Setting] = &[
    Setting {
        key: "socket_path",
        summary: "Path of the herdr control socket flywheel connects to.",
        default_hint: "~/.config/herdr/herdr.sock",
    },
    Setting {
        key: "workspace_root",
        summary: "Directory that holds project checkouts. `flywheel init` and project lookup start here.",
        default_hint: "~/workspace",
    },
    Setting {
        key: "default_timeout_ms",
        summary: "How long send and watch wait, in milliseconds, when you do not pass --wait.",
        default_hint: "1800000 (30 minutes)",
    },
    Setting {
        key: "resonate_url",
        summary: "Resonate server for the durable loop. The loop does not start a server itself.",
        default_hint: "http://localhost:8001",
    },
    Setting {
        key: "on_captain_death",
        summary: "What happens to crews when the captain pane dies: stop (kill crews), continue (leave them), or adopt (a new pin takes the captain alias).",
        default_hint: "stop",
    },
    Setting {
        key: "crew.max",
        summary: "Maximum panes in one crew. A project .flywheel file cannot raise this.",
        default_hint: "5",
    },
    Setting {
        key: "backend.multiplexer",
        summary: "Who owns panes. herdr is the supported multiplexer.",
        default_hint: "herdr",
    },
    Setting {
        key: "backend.mail",
        summary: "Who owns mail. toron is the mail backend.",
        default_hint: "toron",
    },
    Setting {
        key: "mail.toron.url",
        summary: "Toron daemon base URL. Leave unset to run toron inside the flywheel process.",
        default_hint: "unset (embedded)",
    },
    Setting {
        key: "doorbell.url",
        summary: "Doorbell daemon base URL. Leave the [doorbell] section out to run the doorbell inside the process.",
        default_hint: "unset (embedded). When set, http://127.0.0.1:18980",
    },
    Setting {
        key: "agent_mail.name",
        summary: "Agent pin (adjective+noun) used when AGENT_NAME, TORON_AGENT, and FLYWHEEL_MAIL_AS are all unset.",
        default_hint: "unset",
    },
];

fn known_keys() -> String {
    SETTINGS
        .iter()
        .map(|s| s.key)
        .collect::<Vec<_>>()
        .join("|")
}

fn is_known(key: &str) -> bool {
    SETTINGS.iter().any(|s| s.key == key)
}

/// Commented config.toml. Every setting is named, with what it does and the
/// default. Nothing is active until you uncomment it or `config set` it.
pub(crate) fn config_template() -> String {
    // One template. config.sample.toml is the annotated inventory. The header
    // says seed will not replace the file after this first write.
    format!(
        "# Written by `flywheel config seed` because config.toml was missing.\n\
         # Seed never overwrites this file, even with --overwrite all.\n\
         # `flywheel config list` prints each setting below and the value in effect.\n\n{}",
        include_str!("../../config.sample.toml")
    )
}

fn lookup<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let mut cursor = table;
    let mut parts = key.split('.');
    let leaf = parts.next_back()?;
    for part in parts {
        cursor = match cursor.get(part) {
            Some(toml::Value::Table(t)) => t,
            _ => return None,
        };
    }
    cursor.get(leaf)
}

fn effective_setting(cfg: &Config, key: &str) -> String {
    match key {
        "socket_path" => cfg.socket_path(),
        "workspace_root" => cfg.workspace_root().display().to_string(),
        "default_timeout_ms" => cfg.timeout_ms().to_string(),
        "resonate_url" => cfg.resonate_url().unwrap_or_else(|| "(unset)".into()),
        "on_captain_death" => cfg
            .on_captain_death
            .unwrap_or_default()
            .as_str()
            .to_string(),
        "crew.max" => cfg
            .crew
            .as_ref()
            .and_then(|c| c.max)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "5".into()),
        "backend.multiplexer" => cfg
            .backend
            .as_ref()
            .map(|b| b.multiplexer.clone())
            .unwrap_or_else(|| "herdr".into()),
        "backend.mail" => cfg
            .backend
            .as_ref()
            .map(|b| b.mail.clone())
            .unwrap_or_else(|| "toron".into()),
        "mail.toron.url" => cfg
            .mail
            .as_ref()
            .and_then(|m| m.toron.as_ref())
            .and_then(|t| t.url.clone())
            .unwrap_or_else(|| "(embedded)".into()),
        "doorbell.url" => cfg
            .doorbell
            .as_ref()
            .and_then(|d| d.url.clone())
            .unwrap_or_else(|| "(embedded)".into()),
        "agent_mail.name" => cfg
            .agent_mail
            .as_ref()
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| "(unset)".into()),
        _ => "(see file)".into(),
    }
}

fn value_string(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

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
    if !is_known(key) {
        bail!("unknown key: {key} ({})", known_keys());
    }
    if key == "on_captain_death" && !matches!(value, "stop" | "continue" | "adopt") {
        bail!("{key} must be stop, continue, or adopt, got {value:?}");
    }
    let parsed = if key == "default_timeout_ms" || key == "crew.max" {
        toml::Value::Integer(value.parse::<i64>().map_err(|e| {
            anyhow::anyhow!("{key} must be an integer, got {value:?}: {e}")
        })?)
    } else {
        toml::Value::String(value.to_string())
    };
    let mut parts: Vec<&str> = key.split('.').collect();
    let leaf = parts.pop().expect("key has a leaf");
    let mut cursor = table;
    for part in parts {
        let entry = cursor
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        match entry {
            toml::Value::Table(next) => cursor = next,
            _ => bail!("{key}: `{part}` is not a table, refusing to overwrite it"),
        }
    }
    cursor.insert(leaf.to_string(), parsed);
    Ok(())
}

fn seed_config_template(path: &std::path::Path, dry_run: bool) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if !dry_run {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, config_template())?;
    }
    Ok(true)
}

fn cmd_config_seed(json: bool, overwrite: &str, dry_run: bool, yes: bool) -> Result<()> {
    use crate::judgment::PackOverwrite;
    use crate::workflow_catalog::OverwriteScope;
    let scope = OverwriteScope::parse(overwrite)?;
    let pack_scope = PackOverwrite::parse(overwrite)?;
    let root = crate::config::config_dir();
    let pack_dir = root.join("judgment-packs");
    let plan_catalog = crate::workflow_catalog::seed_user_catalog_with(&root, scope, true)?;
    let plan_packs = crate::judgment::seed_default_packs_with(&pack_dir, pack_scope, true)?;
    let mut targets = plan_catalog.planned_overwrites.clone();
    targets.extend(plan_packs.planned_overwrites.iter().cloned());
    if !dry_run {
        crate::output::confirm_destructive("overwrite", &targets, yes, json)?;
    }
    let catalog = crate::workflow_catalog::seed_user_catalog_with(&root, scope, dry_run)?;
    let packs = crate::judgment::seed_default_packs_with(&pack_dir, pack_scope, dry_run)?;
    let config_created = seed_config_template(&root.join("config.toml"), dry_run)?;
    if json {
        crate::output::print_json(&serde_json::json!({
            "root": root.display().to_string(),
            "overwrite": scope.as_str(),
            "dry_run": dry_run,
            "workflows_created": catalog.created_workflows,
            "workflows_kept": catalog.kept_workflows,
            "workflows_overwritten": catalog.overwritten_workflows,
            "stages_created": catalog.created_stages,
            "stages_kept": catalog.kept_stages,
            "stages_overwritten": catalog.overwritten_stages,
            "packs_created": packs.copied,
            "packs_kept": packs.kept,
            "packs_overwritten": packs.overwritten,
            "planned_overwrites": targets,
            "config_toml_created": config_created,
        }));
        return Ok(());
    }
    let verb = if dry_run { "would seed" } else { "seeded" };
    println!(
        "{verb} {} (overwrite: {})",
        root.display(),
        scope.as_str()
    );
    println!(
        "  workflows created {} kept {} overwritten {}",
        catalog.created_workflows.len(),
        catalog.kept_workflows.len(),
        catalog.overwritten_workflows.len()
    );
    println!(
        "  stages created {} kept {} overwritten {}",
        catalog.created_stages.len(),
        catalog.kept_stages.len(),
        catalog.overwritten_stages.len()
    );
    println!(
        "  judgment packs created {} kept {} overwritten {}",
        packs.copied.len(),
        packs.kept.len(),
        packs.overwritten.len()
    );
    println!(
        "  config.toml {}",
        match (dry_run, config_created) {
            (true, true) => "would be created from config.sample.toml",
            (false, true) => "created from config.sample.toml",
            (_, false) => "left as-is (seed never overwrites it)",
        }
    );
    Ok(())
}

fn cmd_config_drift(json: bool) -> Result<()> {
    let root = crate::config::config_dir();
    let mut rows = crate::workflow_catalog::catalog_drift(&root)?;
    for pack in crate::judgment::default_pack_drift(&root.join("judgment-packs")) {
        rows.push(crate::workflow_catalog::CatalogEntryStatus {
            kind: "pack".into(),
            name: pack.name,
            status: pack.status.clone(),
            source: if pack.status == "missing" {
                "builtin"
            } else {
                "user"
            }
            .into(),
            compose_token: false,
        });
    }
    let config_path = root.join("config.toml");
    rows.push(crate::workflow_catalog::CatalogEntryStatus {
        kind: "config".into(),
        name: "config.toml".into(),
        status: if config_path.is_file() {
            "present"
        } else {
            "missing"
        }
        .into(),
        source: "user".into(),
        compose_token: false,
    });
    if json {
        crate::output::print_json(&serde_json::json!({
            "root": root.display().to_string(),
            "writes": false,
            "entries": rows,
        }));
        return Ok(());
    }
    println!(
        "config drift {} (report only, nothing written)",
        root.display()
    );
    for status in ["missing", "stale", "current", "present", "custom"] {
        let group: Vec<_> = rows.iter().filter(|r| r.status == status).collect();
        if group.is_empty() {
            continue;
        }
        println!("  {status} ({})", group.len());
        for row in group.iter().take(8) {
            println!("    {} {}", row.kind, row.name);
        }
        if group.len() > 8 {
            println!("    … {} more", group.len() - 8);
        }
    }
    println!("  repair a stale shipped file: flywheel config seed --overwrite stale --dry-run");
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
            let table: toml::Table = toml::from_str(&raw).unwrap_or_default();
            println!("# {}", path.display());
            println!("# flags and env win over this file. Unset means the default.");
            for setting in SETTINGS {
                let file = lookup(&table, setting.key)
                    .map(value_string)
                    .unwrap_or_else(|| "(unset)".into());
                println!();
                println!("{}", setting.key);
                println!("  {}", setting.summary);
                println!("  default: {}", setting.default_hint);
                println!("  file: {file}");
                println!("  effective: {}", effective_setting(cfg, setting.key));
            }
        }
        ConfigCmd::Seed {
            json,
            overwrite,
            dry_run,
            yes,
        } => cmd_config_seed(*json, overwrite, *dry_run, *yes)?,
        ConfigCmd::Drift { json } => cmd_config_drift(*json)?,
        ConfigCmd::Get { key } => {
            if !is_known(key) {
                bail!("unknown key: {key} ({})", known_keys());
            }
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            let table: toml::Table = toml::from_str(&raw).unwrap_or_default();
            println!(
                "{}",
                lookup(&table, key)
                    .map(value_string)
                    .unwrap_or_default()
            );
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
    use super::{SETTINGS, apply_key, config_template};

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
        for setting in SETTINGS {
            let mut table = toml::Table::new();
            let v = if setting.key == "default_timeout_ms" || setting.key == "crew.max" {
                "5"
            } else if setting.key == "on_captain_death" {
                "stop"
            } else {
                "x"
            };
            apply_key(&mut table, setting.key, v)
                .unwrap_or_else(|e| panic!("advertised key {} rejected: {e}", setting.key));
        }
    }

    #[test]
    fn template_names_every_setting_and_its_default() {
        let text = config_template();
        assert!(text.contains("never overwrites"));
        for needle in [
            "socket_path",
            "resonate_url",
            "on_captain_death",
            "default_timeout_ms",
            "[crew]",
            "[doorbell]",
            "[mail.toron]",
            "[loop]",
        ] {
            assert!(text.contains(needle), "template missing {needle}");
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

//! `flywheel backend` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

/// `flywheel backend` subcommands (ADR-0002 R0.10) — discovery + selection of
/// the pluggable multiplexer/mail backends. clig.dev-disciplined: stdout=data,
/// --json on reads, exit 0/1/2.
#[derive(Subcommand)]
pub(crate) enum BackendCmd {
    /// List compiled-in backends (name, kind, ready|stub, active, feature)
    List,
    /// Show the active multiplexer + mail + config source + health
    Status,
    /// Persist a backend default to config.toml [backend] (env/flag still override)
    Default {
        /// multiplexer | mail
        kind: String,
        /// backend name (herdr|superlogical|toron)
        name: String,
    },
}

/// `flywheel backend` — discovery + selection of the pluggable backends
/// (ADR-0002 R0.10). The `list` subcommand enumerates compiled-in backends
/// (name/kind/ready|stub/active/compiled), `status` shows the active pair +
/// config source + health, and `default` persists a backend to config.toml
/// `[backend]` (env/flag still override at runtime).
///
/// governed-by: ADR-0002 (bd-cli-backend-subcommand)
pub(crate) fn cmd_backend(cfg: &Config, cmd: &BackendCmd) -> Result<()> {
    match cmd {
        BackendCmd::List => {
            let active_mux = crate::backend::resolve_backend_name(cfg);
            let active_mail = crate::backend::resolve_mail_name(cfg);
            let compiled = crate::backend::compiled_backends();
            let mut rows: Vec<Value> = Vec::new();
            // multiplexers
            // Ready only when this binary was built with the rex backend.
            // A default build still lists the name so the env contract is
            // visible, and marks it stub because the feature is off.
            for (name, stub) in [
                ("herdr", false),
                ("superlogical", cfg!(not(feature = "backend-superlogical"))),
            ] {
                rows.push(json!({
                    "name": name,
                    "kind": "multiplexer",
                    "status": if stub { "stub" } else { "ready" },
                    "active": active_mux == name,
                    "compiled_feature": compiled.contains(&name),
                }));
            }
            // mail: toron is the only backend (bd-euav)
            {
                let (name, stub) = ("toron", false);
                rows.push(json!({
                    "name": name,
                    "kind": "mail",
                    "status": if stub { "stub" } else { "ready" },
                    "active": active_mail == name,
                    "compiled_feature": compiled.contains(&name),
                }));
            }
            print_json(&json!({"backends": rows}));
        }
        BackendCmd::Status => {
            let mux = crate::backend::resolve_backend_name(cfg);
            let mail = crate::backend::resolve_mail_name(cfg);
            // config source: env overrides config field over default. Report
            // which layer selected the active name (env/.env/config/default).
            let mux_source = if std::env::var_os("FLYWHEEL_BACKEND").is_some_and(|v| !v.is_empty())
            {
                "env"
            } else if cfg
                .backend
                .as_ref()
                .map(|b| b.multiplexer.as_str())
                .is_some_and(|b| b == mux)
            {
                "config"
            } else {
                "default"
            };
            let mail_source = if std::env::var_os("FLYWHEEL_MAIL").is_some_and(|v| !v.is_empty()) {
                "env"
            } else if cfg
                .backend
                .as_ref()
                .map(|b| b.mail.as_str())
                .is_some_and(|b| b == mail)
            {
                "config"
            } else {
                "default"
            };
            // health: multiplexer = socket reachable; mail = the selected
            // backend's health() (agentmail → AM health_check; toron → toron
            // facade health; not-compiled → false).
            let mux_health = crate::backend::multiplexer_backend(cfg)
                .map(|b| b.agent_list().is_ok())
                .unwrap_or(false);
            let mail_health = crate::backend::mail_backend(cfg)
                .and_then(|m| {
                    crate::backend::run_mail(m.health())
                        .map(|_| true)
                        .map_err(Into::into)
                })
                .unwrap_or(false);
            print_json(&json!({
                "multiplexer": {"name": mux, "source": mux_source, "healthy": mux_health},
                "mail": {"name": mail, "source": mail_source, "healthy": mail_health},
            }));
        }
        BackendCmd::Default { kind, name } => {
            let path = crate::config::config_file();
            let raw = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                String::new()
            };
            let mut cfg_w: crate::config::Config = toml::from_str(&raw).unwrap_or_default();
            let bc = cfg_w.backend.get_or_insert_with(Default::default);
            match kind.as_str() {
                "multiplexer" => bc.multiplexer = name.clone(),
                "mail" => bc.mail = name.clone(),
                other => bail!("unknown backend kind: {other} (multiplexer|mail)"),
            }
            // A `BackendConfig::default()` has empty fields; when the user sets
            // one kind, initialize the OTHER to the currently-resolved default
            // so the written config never contains empty strings (which would
            // deserialize as unset — harmless, but sloppy).
            if bc.multiplexer.is_empty() {
                bc.multiplexer = crate::backend::resolve_backend_name(cfg);
            }
            if bc.mail.is_empty() {
                bc.mail = crate::backend::resolve_mail_name(cfg);
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, toml::to_string_pretty(&cfg_w)?)?;
            println!("default {kind} = {name}");
        }
    }
    Ok(())
}

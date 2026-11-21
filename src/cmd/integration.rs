// governed-by: ADR-0039
//! bd-mdpfr: `flywheel integration install|uninstall|status`. Operator-run,
//! writes ONE owned trampoline entry per agent in the agent's REAL config dir.
//! Never run by spawn; the operator or captain runs it after the proof.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use clap::Subcommand;

use crate::config::Config;
use crate::integration::{
    CLAUDE_EVENTS, CODEX_EVENTS, install_entries, status_of, uninstall_entries,
};

#[derive(clap::Args, Clone)]
pub(crate) struct IntegrationArgs {
    /// Harnesses to act on (default: all).
    pub(crate) harness: Vec<String>,
    /// Operator escape for the fleet-mutation guard.
    #[arg(long)]
    pub(crate) operator: bool,
}

#[derive(Subcommand, Clone)]
pub(crate) enum IntegrationCmd {
    /// Install the owned trampoline entries.
    Install {
        #[command(flatten)]
        args: IntegrationArgs,
    },
    /// Remove exactly the entries install added.
    Uninstall {
        #[command(flatten)]
        args: IntegrationArgs,
    },
    /// Report current, stale or missing per harness.
    Status {
        #[command(flatten)]
        args: IntegrationArgs,
    },
}

impl IntegrationCmd {
    pub(crate) fn args(&self) -> &IntegrationArgs {
        match self {
            Self::Install { args } | Self::Uninstall { args } | Self::Status { args } => args,
        }
    }
}

/// Where the installer reads/writes. Pure data so tests exercise a temp HOME
/// without setting process env (bd-mdpfr).
#[derive(Clone, Debug, Default)]
pub(crate) struct IntegrationPaths {
    pub(crate) home: PathBuf,
    pub(crate) codex_home: PathBuf,
}

impl IntegrationPaths {
    pub(crate) fn from_env() -> Self {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let codex_home = std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".codex"));
        Self { home, codex_home }
    }
}

fn codex_hooks(p: &IntegrationPaths) -> PathBuf {
    p.codex_home.join("hooks.json")
}

fn codex_config(p: &IntegrationPaths) -> PathBuf {
    p.codex_home.join("config.toml")
}

/// Pure TOML edit: make sure `[features] hooks = true` is present, preserving
/// every other line. Codex needs this flag for its hook files to run.
pub fn with_codex_hooks_flag(text: &str) -> String {
    let mut in_features = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_features = t == "[features]";
            continue;
        }
        if in_features
            && t.starts_with("hooks")
            && t.split('=')
                .nth(1)
                .is_some_and(|v| v.trim().trim_matches('"') == "true")
        {
            return text.to_string();
        }
    }
    let mut out = text.trim_end_matches('\n').to_string();
    if let Some(at) = out.find("[features]") {
        let after = at + "[features]".len();
        out.insert_str(after, "\nhooks = true");
    } else {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[features]\nhooks = true");
    }
    out.push('\n');
    out
}

fn grok_hooks(p: &IntegrationPaths) -> PathBuf {
    p.home.join(".grok/hooks/flywheel.json")
}

fn gemini_hooks(p: &IntegrationPaths) -> PathBuf {
    p.home.join(".gemini/config/hooks.json")
}

fn target(kind: &str, p: &IntegrationPaths) -> Option<PathBuf> {
    match crate::harness_registry::canonical(kind).as_str() {
        // bd-9os20: the Claude plugin is delivered per seat with
        // `--plugin-dir` by stack.sh + spawn, not by editing ~/.claude.
        "claude" => None,
        "codex" => Some(codex_hooks(p)),
        "grok" => Some(grok_hooks(p)),
        "antigravity" | "gemini" => Some(gemini_hooks(p)),
        _ => None,
    }
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("{} is not JSON: {e}", path.display()))
}

fn write_atomic(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.is_dir()
    {
        bail!("agent config dir {} does not exist", parent.display());
    }
    let raw = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("flywheel.tmp");
    std::fs::write(&tmp, raw)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn events_for(kind: &str) -> &'static [&'static str] {
    match crate::harness_registry::canonical(kind).as_str() {
        // bd-edva3: antigravity learns the conversation id from a
        // PreInvocation hook report, not from the spawner's environment.
        "antigravity" => &["PreInvocation", "Stop", "PostToolUse"],
        "codex" => CODEX_EVENTS,
        "grok" => &["Stop", "PostToolUse", "SessionEnd"],
        _ => CLAUDE_EVENTS,
    }
}

pub fn cmd_integration(cfg: &Config, cmd: IntegrationCmd) -> Result<()> {
    let operator = cmd.args().operator;
    crate::authority::guard_fleet_mutation(cfg, None, operator, "flywheel integration")?;
    let harnesses: Vec<String> = if cmd.args().harness.is_empty() {
        vec![
            "claude".into(),
            "codex".into(),
            "grok".into(),
            "gemini".into(),
        ]
    } else {
        cmd.args().harness.clone()
    };
    apply_integration(&cmd, &harnesses, &IntegrationPaths::from_env())
}

/// Pure over an injected path map: the whole install/uninstall/status cycle a
/// temp-HOME test exercises without any process env (bd-mdpfr).
fn apply_integration(
    cmd: &IntegrationCmd,
    harnesses: &[String],
    paths: &IntegrationPaths,
) -> Result<()> {
    for kind in harnesses {
        if crate::harness_registry::canonical(kind) == "claude" {
            println!("claude: plugin delivered per seat by --plugin-dir (no config edit)");
            continue;
        }
        let Some(path) = target(kind, paths) else {
            bail!("unsupported harness: {kind}");
        };
        match &cmd {
            IntegrationCmd::Install { .. } => {
                let mut config = if path.is_file() {
                    read_json(&path)?
                } else {
                    serde_json::json!({})
                };
                install_entries(
                    &mut config,
                    &crate::harness_registry::canonical(kind),
                    events_for(kind),
                );
                write_atomic(&path, &config)?;
                if crate::harness_registry::canonical(kind) == "codex" {
                    let cfg_path = codex_config(paths);
                    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
                    std::fs::write(&cfg_path, with_codex_hooks_flag(&raw))?;
                }
                println!("installed {kind} hooks at {}", path.display());
            }
            IntegrationCmd::Uninstall { .. } => {
                if !path.is_file() {
                    println!("{kind}: no config at {}", path.display());
                    continue;
                }
                let mut config = read_json(&path)?;
                let n = uninstall_entries(&mut config, &crate::harness_registry::canonical(kind));
                write_atomic(&path, &config)?;
                println!("removed {n} {kind} hook entry(ies)");
            }
            IntegrationCmd::Status { .. } => {
                let config = if path.is_file() {
                    read_json(&path)?
                } else {
                    serde_json::Value::Null
                };
                let state = status_of(
                    &config,
                    &crate::harness_registry::canonical(kind),
                    events_for(kind),
                );
                println!("{kind}: {state:?} ({})", path.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(tag: &str) -> (std::path::PathBuf, IntegrationPaths) {
        let home = std::env::temp_dir().join(format!("integration-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let paths = IntegrationPaths {
            home: home.clone(),
            codex_home: home.join(".codex"),
        };
        (home, paths)
    }

    #[test]
    fn install_writes_under_temp_home_and_is_idempotent() {
        let (home, paths) = temp_paths("install");
        for dir in [".codex", ".grok/hooks", ".gemini/config"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        let harnesses = vec!["codex".into(), "grok".into(), "gemini".into()];
        let install = IntegrationCmd::Install {
            args: IntegrationArgs {
                harness: vec![],
                operator: false,
            },
        };
        apply_integration(&install, &harnesses, &paths).unwrap();
        assert!(home.join(".codex/hooks.json").is_file());
        assert!(home.join(".grok/hooks/flywheel.json").is_file());
        assert!(home.join(".gemini/config/hooks.json").is_file());
        assert_eq!(
            std::fs::read_to_string(codex_config(&paths)).unwrap(),
            "[features]\nhooks = true\n"
        );
        // Install twice changes nothing.
        let before: Vec<_> = harnesses
            .iter()
            .map(|h| std::fs::read(target(h, &paths).unwrap()).unwrap())
            .collect();
        apply_integration(&install, &harnesses, &paths).unwrap();
        let after: Vec<_> = harnesses
            .iter()
            .map(|h| std::fs::read(target(h, &paths).unwrap()).unwrap())
            .collect();
        assert_eq!(before, after, "install twice is byte-identical");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn install_refuses_when_the_config_dir_is_missing() {
        let (home, paths) = temp_paths("refuse");
        // No .codex dir created: install must refuse, not mkdir it.
        let install = IntegrationCmd::Install {
            args: IntegrationArgs {
                harness: vec!["codex".into()],
                operator: false,
            },
        };
        assert!(apply_integration(&install, &["codex".into()], &paths).is_err());
        assert!(!home.join(".codex/hooks.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn uninstall_restores_and_status_tracks() {
        let (home, paths) = temp_paths("uninstall");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let rex = serde_json::json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "rex session-start", "timeout": 5}]}]}});
        std::fs::write(codex_hooks(&paths), rex.to_string()).unwrap();
        let harnesses = vec!["codex".into()];
        let install = IntegrationCmd::Install {
            args: IntegrationArgs {
                harness: vec![],
                operator: false,
            },
        };
        apply_integration(&install, &harnesses, &paths).unwrap();
        let status = IntegrationCmd::Status {
            args: IntegrationArgs {
                harness: vec![],
                operator: false,
            },
        };
        apply_integration(&status, &harnesses, &paths).unwrap();
        let uninstall = IntegrationCmd::Uninstall {
            args: IntegrationArgs {
                harness: vec![],
                operator: false,
            },
        };
        apply_integration(&uninstall, &harnesses, &paths).unwrap();
        let restored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(codex_hooks(&paths)).unwrap()).unwrap();
        assert_eq!(restored, rex, "rex entry survives byte-for-byte");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn codex_hooks_flag_is_added_once_and_preserves_other_config() {
        let original = "[model]\nname = \"gpt\"\n";
        let with = with_codex_hooks_flag(original);
        assert!(with.contains("[features]\nhooks = true"));
        assert!(with.contains("[model]\nname = \"gpt\""));
        assert_eq!(
            with_codex_hooks_flag(&with),
            with,
            "install twice is idempotent"
        );
    }

    #[test]
    fn codex_hooks_flag_uses_an_existing_features_section() {
        let original = "[features]\nother = true\n";
        let with = with_codex_hooks_flag(original);
        assert_eq!(with, "[features]\nhooks = true\nother = true\n");
    }
}

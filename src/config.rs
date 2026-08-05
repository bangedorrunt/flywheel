//! XDG configuration: ~/.config/flywheel/{config.toml, recipes.toml, workflows.toml, pipelines.toml, policy.toml}
//! Precedence: flags > env (FLYWHEEL_SOCKET / FLYWHEEL_ROOT / FLYWHEEL_TIMEOUT /
//! FLYWHEEL_BACKEND / FLYWHEEL_MAIL) > `.env` (dotenvy, project-local) >
//! config file > defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_TIMEOUT_MS: u64 = 1_800_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailConfig {
    /// agent-mail server base URL, e.g. http://127.0.0.1:8765
    pub url: Option<String>,
    /// bearer token for the agent-mail MCP endpoint
    pub token: Option<String>,
    /// agent identity (adjective+noun) used when registering/messaging
    pub name: Option<String>,
}

/// Backend selection (ADR-0002). `[backend] multiplexer` / `[backend] mail` in
/// config.toml; overridden by `FLYWHEEL_BACKEND` / `FLYWHEEL_MAIL` env (which
/// may themselves be set by `.env`). Defaults: herdr / agentmail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendConfig {
    /// multiplexer backend name (`herdr` default; `superlogical`/`tmux` future)
    pub multiplexer: String,
    /// mail backend name (`agentmail` default; `toron` future)
    pub mail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// herdr socket path (default ~/.config/herdr/herdr.sock)
    pub socket_path: Option<String>,
    /// default project root (default ~/workspace)
    pub workspace_root: Option<String>,
    /// default --wait timeout in ms for send/watch (default 1800000)
    pub default_timeout_ms: Option<u64>,
    /// agent-mail coordination server
    pub agent_mail: Option<MailConfig>,
    /// Resonate server URL for the durable loop (default http://localhost:8001)
    pub resonate_url: Option<String>,
    /// short name -> herdr agent kind (default: oc=opencode, pi=pi, gk=grok)
    pub kinds: Option<BTreeMap<String, String>>,
    /// backend selection (ADR-0002)
    pub backend: Option<BackendConfig>,
}

impl Config {
    pub fn load() -> Result<Self> {
        // Load .env first (non-fatal if absent). dotenvy does NOT override
        // already-set env vars, so a real env var wins over .env (ADR-0002 §3.3
        // precedence: flag > env > .env > config.toml > default).
        let _ = dotenvy::dotenv();
        let path = config_file();
        let mut cfg: Config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?
        } else {
            Config::default()
        };
        // env overrides
        if let Ok(v) = std::env::var("FLYWHEEL_SOCKET") {
            cfg.socket_path = Some(v);
        }
        if let Ok(v) = std::env::var("FLYWHEEL_ROOT") {
            cfg.workspace_root = Some(v);
        }
        if let Ok(v) = std::env::var("FLYWHEEL_TIMEOUT") {
            cfg.default_timeout_ms = Some(v.parse().unwrap_or(DEFAULT_TIMEOUT_MS));
        }
        if let Ok(v) = std::env::var("RESONATE_URL") {
            cfg.resonate_url = Some(v);
        }
        Ok(cfg)
    }

    pub fn socket_path(&self) -> String {
        self.socket_path
            .clone()
            .or_else(|| std::env::var("HERDR_SOCKET_PATH").ok())
            .unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap_or_else(|| Path::new("~").to_path_buf())
                    .join("herdr/herdr.sock")
                    .to_string_lossy()
                    .into_owned()
            })
    }

    pub fn workspace_root(&self) -> PathBuf {
        self.workspace_root
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/"))
                    .join("workspace")
            })
    }

    pub fn resonate_url(&self) -> Option<String> {
        self.resonate_url.clone()
    }

    pub fn timeout_ms(&self) -> u64 {
        self.default_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    /// Resolve a short agent name (oc/pi/gk) or full kind to a herdr agent kind.
    pub fn resolve_kind(&self, short: &str) -> String {
        if let Some(map) = &self.kinds {
            if let Some(k) = map.get(short) {
                return k.clone();
            }
        }
        match short {
            "oc" => "opencode".into(),
            "pi" => "pi".into(),
            "gk" => "grok".into(),
            other => other.into(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("flywheel")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".local/state")
        })
        .join("flywheel")
}

// ---- recipes ----

/// A spawn recipe: named counts of agent kinds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recipe {
    pub oc: Option<u32>,
    pub pi: Option<u32>,
    pub gk: Option<u32>,
    /// extra kinds: kind name -> count (e.g. claude = 1)
    #[serde(flatten)]
    pub extra: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recipes {
    #[serde(flatten)]
    pub recipes: BTreeMap<String, Recipe>,
}

impl Recipes {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("recipes.toml");
        if !path.exists() {
            return Ok(Recipes::default());
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }
}

// ---- workflows ----

/// One step of a workflow: either a send (to agents) or a wait.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WorkflowStep {
    Send {
        /// target: "all" or comma-separated shorts (oc,pi,gk)
        to: String,
        /// stage name (p1|p2|p4|p5|p6|p8) OR raw message
        #[serde(default)]
        stage: Option<String>,
        #[serde(default)]
        msg: Option<String>,
    },
    Wait {
        /// agent status to wait for
        until: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workflows {
    #[serde(flatten)]
    pub workflows: BTreeMap<String, Vec<WorkflowStep>>,
}

impl Workflows {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("workflows.toml");
        if !path.exists() {
            return Ok(Workflows::default());
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }
}

// ---- pipelines ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pipelines {
    #[serde(flatten)]
    pub pipelines: BTreeMap<String, Vec<String>>,
}

impl Pipelines {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("pipelines.toml");
        if !path.exists() {
            return Ok(Pipelines::default());
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }
}

// ---- policy (safety) ----

/// Destructive-command guard: deny patterns checked against send messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    /// regex patterns that are refused in send messages without --force
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Policy {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("policy.toml");
        if !path.exists() {
            return Ok(Policy::default());
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }

    /// Returns the first matching deny pattern, if any.
    pub fn violation(&self, msg: &str) -> Option<&str> {
        self.deny
            .iter()
            .find(|p| {
                regex_lite::Regex::new(p)
                    .map(|re| re.is_match(msg))
                    .unwrap_or_else(|_| msg.contains(p.as_str()))
            })
            .map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_kind_defaults() {
        let c = Config::default();
        assert_eq!(c.resolve_kind("oc"), "opencode");
        assert_eq!(c.resolve_kind("pi"), "pi");
        assert_eq!(c.resolve_kind("gk"), "grok");
        assert_eq!(c.resolve_kind("claude"), "claude");
    }

    #[test]
    fn resolve_kind_custom() {
        let mut kinds = BTreeMap::new();
        kinds.insert("oc".into(), "opencode".into());
        kinds.insert("qwen".into(), "qwen".into());
        let c = Config {
            kinds: Some(kinds),
            ..Default::default()
        };
        assert_eq!(c.resolve_kind("qwen"), "qwen");
        assert_eq!(c.resolve_kind("oc"), "opencode");
    }

    #[test]
    fn policy_violation_matches() {
        let p = Policy {
            deny: vec!["rm -rf".into(), "git push --force".into()],
        };
        assert_eq!(p.violation("run rm -rf /"), Some("rm -rf"));
        assert_eq!(p.violation("refactor the auth layer"), None);
    }

    #[test]
    fn recipes_parse() {
        let raw = r#"
[swarm]
oc = 1
pi = 1
gk = 1

[qa]
oc = 2
"#;
        let r: Recipes = toml::from_str(raw).unwrap();
        assert_eq!(r.recipes["swarm"].oc, Some(1));
        assert_eq!(r.recipes["qa"].oc, Some(2));
    }

    #[test]
    fn workflows_parse() {
        let raw = r#"
daily = [
  { type = "send", to = "all", stage = "p1" },
  { type = "wait", until = "idle", timeout_ms = 600000 },
  { type = "send", to = "oc", stage = "p6" },
]
"#;
        let w: Workflows = toml::from_str(raw).unwrap();
        let steps = &w.workflows["daily"];
        assert_eq!(steps.len(), 3);
        match &steps[0] {
            WorkflowStep::Send { stage, to, .. } => {
                assert_eq!(stage.as_deref(), Some("p1"));
                assert_eq!(to, "all");
            }
            _ => panic!("expected send"),
        }
    }
}

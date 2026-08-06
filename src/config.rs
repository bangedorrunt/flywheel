//! XDG configuration: ~/.config/flywheel/{config.toml, recipes.toml, workflows.toml, pipelines.toml, policy.toml}
//! Precedence: flags > env (FLYWHEEL_SOCKET / FLYWHEEL_ROOT / FLYWHEEL_TIMEOUT /
//! FLYWHEEL_BACKEND / FLYWHEEL_MAIL) > `.env` (dotenvy, project-local) >
//! config file > defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_TIMEOUT_MS: u64 = 1_800_000;

/// `[agent_mail]` section — the SHARED mail identity (ADR-0003 V3). Keeps only
/// the identity/name (and shared token); per-backend transport lives in
/// `[mail.agentmail]` (real agent-mail server) and `[mail.toron]` (toron
/// daemon). `url` is tolerated on load for legacy-config migration but is no
/// longer the source of truth for either backend's routing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailConfig {
    /// Legacy field: the pre-split single transport URL. Backends read their
    /// own `[mail.*]` section now; kept so old configs still parse. The toron
    /// facade uses it as a fallback until migrated (bd-am3).
    pub url: Option<String>,
    /// bearer token (shared; also carried by each `[mail.*]` section)
    pub token: Option<String>,
    /// agent identity (adjective+noun) used when registering/messaging
    pub name: Option<String>,
}

/// One per-backend transport section under `[mail.*]` (ADR-0003 V3): the
/// real agent-mail server (`[mail.agentmail]`, 127.0.0.1:8765) and the toron
/// daemon (`[mail.toron]`, 127.0.0.1:18080; absent → embedded in-process).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailServerConfig {
    /// daemon base URL (e.g. `http://127.0.0.1:8765`). Unset → default
    /// (agent-mail: the standard port; toron: embedded in-process).
    pub url: Option<String>,
    /// bearer token for the daemon's MCP endpoint
    pub token: Option<String>,
}

/// `[mail]` section — the per-backend transport table (ADR-0003 V3). Splitting
/// the old shared `[agent_mail] url` fixes the BUG1 misroute: each backend
/// reads ONLY its own section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailSections {
    /// `[mail.agentmail]` — real agent-mail server (8765).
    pub agentmail: Option<MailServerConfig>,
    /// `[mail.toron]` — toron daemon (18080); absent → embedded in-process.
    pub toron: Option<MailServerConfig>,
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
    /// optional mail config (in-process by default; remote daemon when
    /// `agent_mail.url` is set — bd-o8j.2/.3)
    pub agent_mail: Option<MailConfig>,
    /// per-backend mail transport (ADR-0003 V3): `[mail.agentmail]` for the
    /// real agent-mail server, `[mail.toron]` for the toron daemon
    pub mail: Option<MailSections>,
    /// Resonate server URL for the durable loop (default http://localhost:8001)
    pub resonate_url: Option<String>,
    /// short name -> herdr agent kind (default: oc=opencode, pi=pi, gk=grok)
    pub kinds: Option<BTreeMap<String, String>>,
    /// backend selection (ADR-0002)
    pub backend: Option<BackendConfig>,
    /// durable-loop self-heal knobs (bd-event-self-heal-unknown)
    #[serde(default)]
    pub r#loop: Option<LoopConfig>,
}

/// `[loop]` section: self-heal thresholds for the durable swarm loop.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopConfig {
    /// consecutive `unknown` polls before Diagnose (default 3)
    pub unknown_diagnose_threshold: Option<u32>,
    /// max respawns per agent per loop run before escalate-blocked (default 2)
    pub respawn_budget: Option<u32>,
}

impl Config {
    /// Diagnose threshold (default 3).
    pub fn unknown_diagnose_threshold(&self) -> u32 {
        self.r#loop
            .as_ref()
            .and_then(|l| l.unknown_diagnose_threshold)
            .unwrap_or(3)
    }

    /// Respawn budget (default 2).
    pub fn respawn_budget(&self) -> u32 {
        self.r#loop
            .as_ref()
            .and_then(|l| l.respawn_budget)
            .unwrap_or(2)
    }

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
        // agent-mail env surface (the factory's error message documents
        // FLYWHEEL_AGENTMAIL_URL/TOKEN — make good on it): FLYWHEEL_AGENTMAIL_URL
        // / FLYWHEEL_AGENTMAIL_TOKEN feed `[mail.agentmail]` ONLY (ADR-0003 V3 —
        // never toron), merging over config.toml; FLYWHEEL_MAIL_NAME feeds the
        // shared `[agent_mail]` identity.
        let (am_url, am_token, mail_name) = Config::mail_env_overrides();
        if am_url.is_some() || am_token.is_some() {
            let mut am = cfg
                .mail
                .as_mut()
                .map(|m| m.agentmail.clone())
                .unwrap_or_default()
                .unwrap_or_default();
            if let Some(v) = am_url {
                am.url = Some(v);
            }
            if let Some(v) = am_token {
                am.token = Some(v);
            }
            cfg.mail.get_or_insert_with(Default::default).agentmail = Some(am);
        }
        if let Some(v) = mail_name {
            let mut mail_cfg = cfg.agent_mail.clone().unwrap_or_default();
            mail_cfg.name = Some(v);
            cfg.agent_mail = Some(mail_cfg);
        }
        Ok(cfg)
    }

    /// Agent-mail env overrides: `FLYWHEEL_AGENTMAIL_URL` /
    /// `FLYWHEEL_AGENTMAIL_TOKEN` feed `[mail.agentmail]`; `FLYWHEEL_MAIL_NAME`
    /// feeds the shared `[agent_mail]` identity. Empty values count as unset
    /// (matching the factory's `.filter(|s| !s.is_empty())` rule).
    pub fn mail_env_overrides() -> (Option<String>, Option<String>, Option<String>) {
        (
            std::env::var("FLYWHEEL_AGENTMAIL_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            std::env::var("FLYWHEEL_AGENTMAIL_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            std::env::var("FLYWHEEL_MAIL_NAME")
                .ok()
                .filter(|s| !s.is_empty()),
        )
    }

    /// Effective agent-mail (AM) transport config: `[mail.agentmail]` url/token,
    /// url defaulting to the standard agent-mail port (`http://127.0.0.1:8765`)
    /// so the zero-config default build constructs lazily (ADR-0003 V3/V4 —
    /// ops then report the actionable "not reachable" error if `am` is down).
    /// The token falls back to the shared `[agent_mail] token` when the
    /// per-backend section omits one (ADR-0003 V3: `[agent_mail]` keeps
    /// "token if shared"); the URL never falls back to `[agent_mail] url`
    /// (that is the BUG1 misroute this split fixes).
    pub fn agentmail_server(&self) -> MailServerConfig {
        let sec = self.mail.as_ref().and_then(|m| m.agentmail.as_ref());
        let shared = self.agent_mail.as_ref();
        MailServerConfig {
            url: Some(
                sec.and_then(|c| c.url.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "http://127.0.0.1:8765".to_string()),
            ),
            token: sec
                .and_then(|c| c.token.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    shared
                        .and_then(|c| c.token.clone())
                        .filter(|s| !s.is_empty())
                }),
        }
    }

    /// Effective toron facade config: identity `name` from the shared
    /// `[agent_mail]`, transport url/token from `[mail.toron]`. When
    /// `[mail.toron]` is absent, falls back to the legacy `[agent_mail] url`
    /// (a pre-split config keeps its toron remote until bd-am3 migrates the
    /// block); no url anywhere → embedded in-process toron.
    pub fn toron_mail_config(&self) -> MailConfig {
        let sec = self.mail.as_ref().and_then(|m| m.toron.as_ref());
        let legacy = self.agent_mail.as_ref();
        MailConfig {
            url: sec
                .and_then(|c| c.url.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| legacy.and_then(|c| c.url.clone()).filter(|s| !s.is_empty())),
            token: sec
                .and_then(|c| c.token.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    legacy
                        .and_then(|c| c.token.clone())
                        .filter(|s| !s.is_empty())
                }),
            name: legacy.and_then(|c| c.name.clone()),
        }
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

    /// bd-docs-backend-abstraction: ship `.env.example`; real `.env` stays gitignored.
    #[test]
    fn env_example_shipped_and_env_gitignored() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let example = root.join(".env.example");
        assert!(
            example.is_file(),
            ".env.example missing at {}",
            example.display()
        );
        let body = std::fs::read_to_string(&example).unwrap();
        assert!(
            body.contains("FLYWHEEL_BACKEND") && body.contains("FLYWHEEL_MAIL"),
            ".env.example must document backend selection vars"
        );
        let gi = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(
            gi.lines().any(|l| l.trim() == ".env"),
            ".gitignore must contain exact line `.env`"
        );
    }

    /// bd-docs-collab-spine-recipes: no stale poll-only / v18 framing.
    #[test]
    fn collab_docs_have_no_stale_tokens() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for rel in ["AGENTS.md", "README.md"] {
            let body = std::fs::read_to_string(root.join(rel)).unwrap_or_default();
            let lower = body.to_ascii_lowercase();
            assert!(
                !lower.contains("protocol v18"),
                "{rel} still mentions protocol v18"
            );
            assert!(
                !lower.contains("state-poll stream"),
                "{rel} still mentions state-poll stream"
            );
        }
        assert!(
            root.join("docs/collab/recipes.md").is_file(),
            "recipe catalog missing"
        );
    }

    // Env-var mutation is process-global; serialize tests that touch it
    // (same pattern as the backend factory tests).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn mail_env_overrides_read_all_vars() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("FLYWHEEL_AGENTMAIL_URL", "http://127.0.0.1:8765");
        std::env::set_var("FLYWHEEL_AGENTMAIL_TOKEN", "secret");
        std::env::set_var("FLYWHEEL_MAIL_NAME", "Orchid");
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:8765"));
        assert_eq!(token.as_deref(), Some("secret"));
        assert_eq!(name.as_deref(), Some("Orchid"));
        std::env::remove_var("FLYWHEEL_AGENTMAIL_URL");
        std::env::remove_var("FLYWHEEL_AGENTMAIL_TOKEN");
        std::env::remove_var("FLYWHEEL_MAIL_NAME");
    }

    #[test]
    fn mail_env_overrides_ignore_empty_values() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("FLYWHEEL_AGENTMAIL_URL", "");
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!(url, None, "empty url counts as unset");
        assert_eq!(token, None);
        assert_eq!(name, None);
        std::env::remove_var("FLYWHEEL_AGENTMAIL_URL");
    }

    #[test]
    fn mail_env_overrides_nothing_set() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("FLYWHEEL_AGENTMAIL_URL");
        std::env::remove_var("FLYWHEEL_AGENTMAIL_TOKEN");
        std::env::remove_var("FLYWHEEL_MAIL_NAME");
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!((url, token, name), (None, None, None));
    }

    /// Per-backend config split (ADR-0003 V3, BUG1): `[mail.agentmail]` and
    /// `[mail.toron]` carry their OWN url — the shared `[agent_mail]` url can
    /// never route the wrong backend. `agentmail_server()` defaults to the
    /// standard AM port (zero-config default build); `toron_mail_config()`
    /// defaults to embedded in-process (no url).
    #[test]
    fn per_backend_config_split_isolates_urls() {
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://127.0.0.1:18080".into()), // legacy (ignored)
                token: Some("shared".into()),
                name: Some("Orchid".into()),
            }),
            mail: Some(MailSections {
                agentmail: Some(MailServerConfig {
                    url: Some("http://127.0.0.1:8765".into()),
                    token: Some("am-token".into()),
                }),
                toron: Some(MailServerConfig {
                    url: Some("http://127.0.0.1:18080".into()),
                    token: Some("toron-token".into()),
                }),
            }),
            ..Default::default()
        };
        // AM reads ONLY [mail.agentmail]; the legacy [agent_mail] url (toron!)
        // must NOT leak into it — that was the BUG1 misroute.
        let am = cfg.agentmail_server();
        assert_eq!(am.url.as_deref(), Some("http://127.0.0.1:8765"));
        assert_eq!(am.token.as_deref(), Some("am-token"));
        // toron reads [mail.toron] + shared identity name
        let toron = cfg.toron_mail_config();
        assert_eq!(toron.url.as_deref(), Some("http://127.0.0.1:18080"));
        assert_eq!(toron.token.as_deref(), Some("toron-token"));
        assert_eq!(toron.name.as_deref(), Some("Orchid"));
    }

    /// Legacy-config bridge: an old `[agent_mail] url` (a toron URL) keeps the
    /// toron facade remote until bd-am3 migrates the block; agentmail still
    /// defaults to the AM port and never sees the legacy url (BUG1). The
    /// shared `[agent_mail] token` IS inherited by agentmail (ADR-0003 V3:
    /// "[agent_mail] keeps … token if shared").
    #[test]
    fn toron_falls_back_to_legacy_agent_mail_url_but_agentmail_does_not() {
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://127.0.0.1:18080".into()),
                token: Some("legacy".into()),
                name: Some("Orchid".into()),
            }),
            ..Default::default()
        };
        let toron = cfg.toron_mail_config();
        assert_eq!(toron.url.as_deref(), Some("http://127.0.0.1:18080"));
        assert_eq!(toron.token.as_deref(), Some("legacy"));
        assert_eq!(toron.name.as_deref(), Some("Orchid"));
        let am = cfg.agentmail_server();
        assert_eq!(
            am.url.as_deref(),
            Some("http://127.0.0.1:8765"),
            "AM never inherits the legacy toron url"
        );
        assert_eq!(
            am.token.as_deref(),
            Some("legacy"),
            "AM inherits the SHARED token (V3)"
        );
    }

    /// Zero-config default build: no sections at all → AM points at the
    /// standard port, toron is embedded in-process.
    #[test]
    fn zero_config_defaults_agentmail_port_and_embedded_toron() {
        let cfg = Config::default();
        let am = cfg.agentmail_server();
        assert_eq!(am.url.as_deref(), Some("http://127.0.0.1:8765"));
        assert_eq!(am.token, None);
        let toron = cfg.toron_mail_config();
        assert_eq!(toron.url, None, "no url -> embedded in-process toron");
        assert_eq!(toron.name, None);
    }
}

//! XDG configuration: ~/.config/flywheel/{config.toml, recipes.toml, workflows.toml, pipelines.toml, policy.toml}
//! Precedence: flags > env (FLYWHEEL_SOCKET / FLYWHEEL_ROOT / FLYWHEEL_TIMEOUT /
//! FLYWHEEL_BACKEND / FLYWHEEL_MAIL) > `.env` (dotenvy, project-local) >
//! config file > defaults.
//!
//! Config path convergence (ADR-0003 V5): the REAL XDG dir is
//! `$XDG_CONFIG_HOME || ~/.config/flywheel` — NOT `dirs::config_dir()`, which
//! returns `~/Library/Application Support` on macOS (the doc used to lie).
//! `Config::load()` dual-reads XDG first with the legacy macOS path
//! (`~/Library/Application Support/flywheel/config.toml`) as fallback; on the
//! first successful legacy load it COPIES (not atomic-moves — rename across
//! volumes fails, crash mid-move loses config) to XDG, keeps the legacy file
//! as backup, and all later writes go through XDG (`config_file()`).

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
        // Dual-read (ADR-0003 V5): XDG first; the legacy macOS path as a
        // fallback. On the first successful legacy load, COPY to XDG and keep
        // the legacy file as backup; every later run reads/writes XDG.
        let xdg = config_file();
        let legacy = legacy_config_file();
        let mut cfg: Config = if xdg.exists() {
            let raw =
                std::fs::read_to_string(&xdg).with_context(|| format!("read {}", xdg.display()))?;
            toml::from_str(&raw).with_context(|| format!("parse {}", xdg.display()))?
        } else if legacy.exists() {
            let raw = std::fs::read_to_string(&legacy)
                .with_context(|| format!("read {}", legacy.display()))?;
            let parsed: Config =
                toml::from_str(&raw).with_context(|| format!("parse {}", legacy.display()))?;
            // Write-through to XDG thereafter (copy, not move — V5).
            migrate_legacy_config(&legacy, &xdg);
            parsed
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
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let base = xdg.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".config")
    });
    base.join("flywheel")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Legacy pre-XDG config dir: on macOS `dirs::config_dir()` returned
/// `~/Library/Application Support`, so old flywheel configs live there.
/// `Config::load()` falls back to this path and migrates it to XDG on first
/// success (ADR-0003 V5). On non-macOS this is just a path that won't exist
/// (XDG is the canonical location there already).
pub fn legacy_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("Library/Application Support/flywheel")
}

pub fn legacy_config_file() -> PathBuf {
    legacy_config_dir().join("config.toml")
}

/// Copy the legacy config to the real XDG path (COPY, never atomic-move —
/// ADR-0003 V5: rename across volumes fails and a crash mid-move loses the
/// config; copying keeps the legacy file as a backup). Creates the XDG dir.
/// Best-effort: a copy failure must not fail startup (the legacy file remains
/// the load source until a later run succeeds).
fn migrate_legacy_config(legacy: &Path, xdg: &Path) {
    if let Err(e) = (|| -> std::io::Result<()> {
        if let Some(parent) = xdg.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(legacy, xdg)?;
        Ok(())
    })() {
        eprintln!(
            "flywheel: could not copy legacy config {} -> {}: {e}",
            legacy.display(),
            xdg.display()
        );
    }
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

    // ---- ADR-0003 V5: XDG config convergence ----
    //
    // `config_dir()` returns the REAL XDG path ($XDG_CONFIG_HOME ||
    // ~/.config/flywheel), NOT `dirs::config_dir()` (macOS Application
    // Support — the doc used to lie). These tests isolate HOME/XDG in a temp
    // dir (serialized via ENV_LOCK — env mutation is process-global).

    /// Real XDG dir: `$XDG_CONFIG_HOME` wins; else `~/.config/flywheel`.
    #[test]
    fn config_dir_is_real_xdg() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = std::env::temp_dir().join(format!("flywheel-cfg-xdg-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        assert_eq!(config_dir(), tmp.join("flywheel"));
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// No XDG_CONFIG_HOME → `~/.config/flywheel` (NOT macOS Application
    /// Support, which is what `dirs::config_dir()` returns).
    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::remove_var("XDG_CONFIG_HOME");
        let home = std::env::temp_dir().join(format!("flywheel-cfg-home-{}", std::process::id()));
        std::env::set_var("HOME", &home);
        assert_eq!(
            config_dir(),
            home.join(".config/flywheel"),
            "XDG default is ~/.config/flywheel, not dirs::config_dir()"
        );
        std::env::remove_var("HOME");
    }

    /// `Config::load()` dual-reads: XDG wins when present; a legacy macOS
    /// config is read as a fallback and COPIED (not moved) to XDG, keeping the
    /// legacy file as backup. The live `[agent_mail]` block survives the copy.
    #[test]
    fn load_migrates_legacy_config_to_xdg_copying() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Isolate HOME + XDG in temp dirs (clean; no real config interference).
        let home =
            std::env::temp_dir().join(format!("flywheel-legacy-home-{}", std::process::id()));
        let xdg = std::env::temp_dir().join(format!("flywheel-xdg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&xdg);
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        // legacy macOS path + live [agent_mail] block (ADR-0003 V5)
        let legacy_dir = home.join("Library/Application Support/flywheel");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join("config.toml"),
            "[agent_mail]\nname = \"Orchid\"\n",
        )
        .unwrap();

        let cfg = Config::load().expect("load falls back to legacy");
        assert_eq!(
            cfg.agent_mail.as_ref().and_then(|m| m.name.as_deref()),
            Some("Orchid"),
            "live [agent_mail] block survives migration"
        );
        // Copied to XDG; legacy kept as backup (NOT moved).
        let xdg_file = xdg.join("flywheel/config.toml");
        assert!(xdg_file.exists(), "legacy config copied to XDG");
        assert!(
            legacy_dir.join("config.toml").exists(),
            "legacy file kept as backup (copy, not move)"
        );
        let copied = std::fs::read_to_string(&xdg_file).unwrap();
        assert!(copied.contains("Orchid"), "copied body intact");
        std::env::remove_var("HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// XDG beats legacy when both exist (precedence XDG > legacy > default).
    #[test]
    fn load_prefers_xdg_over_legacy() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home = std::env::temp_dir().join(format!("flywheel-pref-home-{}", std::process::id()));
        let xdg = std::env::temp_dir().join(format!("flywheel-pref-xdg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&xdg);
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::fs::create_dir_all(home.join("Library/Application Support/flywheel")).unwrap();
        std::fs::write(
            home.join("Library/Application Support/flywheel/config.toml"),
            "[agent_mail]\nname = \"Legacy\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(xdg.join("flywheel")).unwrap();
        std::fs::write(
            xdg.join("flywheel/config.toml"),
            "[agent_mail]\nname = \"Xdg\"\n",
        )
        .unwrap();

        let cfg = Config::load().expect("load reads XDG");
        assert_eq!(
            cfg.agent_mail.as_ref().and_then(|m| m.name.as_deref()),
            Some("Xdg"),
            "XDG wins over legacy"
        );
        std::env::remove_var("HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}

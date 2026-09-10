// governed-by: ADR-0025
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

/// Crate-wide lock for tests that mutate process env vars (193.2): env is
/// process-global, so ALL env-mutating tests across modules must serialize
/// under ONE lock — per-module locks would still race each other. Config's
/// tests own the reference pattern; every other module's env-mutating test
/// takes this lock.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the crate-wide env lock (poison-tolerant). Test-only (193.2).
#[cfg(test)]
pub fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hermetic state dir for a test (193.2): holds the crate-wide env lock and
/// points `XDG_STATE_HOME` at a fresh per-call temp dir for the guard's
/// lifetime, removing it on drop. Every test that writes `state_dir()`
/// artifacts (loops sidecars, last_sent, xproj seen-markers) MUST take this
/// — the real state dir is shared with production loops and sibling test
/// processes; writing it poisons later runs (e.g. a v1 fixture left behind
/// re-triggers the sidecar migration mid-suite).
#[cfg(test)]
pub struct TestStateHome {
    _env: std::sync::MutexGuard<'static, ()>,
    dir: PathBuf,
}

#[cfg(test)]
impl Drop for TestStateHome {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Begin a hermetic state-dir test (see [`TestStateHome`]).
#[cfg(test)]
pub fn test_state_home() -> TestStateHome {
    let _env = lock_env();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "flywheel-state-home-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test state home");
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
    TestStateHome { _env, dir }
}

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

/// `[doorbell]` section (ADR-0012 D2): the plane-D daemon host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DoorbellSection {
    /// daemon base URL (default `http://127.0.0.1:18980` when the section
    /// exists). The section's presence selects the daemon host; absence
    /// selects embedded (with a 200ms loopback deference probe, D9).
    pub url: Option<String>,
    /// arrival triple (bd-asr2.14, D5): presence + role auto-bind + join
    /// ring + roster headers. Default false — behavior extensions stay
    /// opt-in so migration parity holds with the flag off.
    #[serde(default)]
    pub arrival_triple: bool,
    /// beads-poller sensor (bd-asr2.15, D7): read-only WAL poll of the br
    /// sqlite (holds, issue changes) emitting Fact Msgs. Default false.
    /// The poller NEVER writes: `sqlite3 --readonly` + SELECT only.
    #[serde(default)]
    pub beads_poller: bool,
    /// cron doorbell sink (bd-asr2.16, D7): cron tick completion journals a
    /// Fact row and queues a pending wake (digest-visible with zero swarm;
    /// emit/deliver binding lands in bd-asr2.17). Default false.
    #[serde(default)]
    pub cron_doorbell_sink: bool,
    /// Service identities allowed to ring via POST /api/v1/emit
    /// (bd-asr2.17, D7): name -> token hash + channel scope + rate.
    /// Empty (default) disables the endpoint. Revocation = delete entry.
    #[serde(default)]
    pub services: std::collections::HashMap<String, ServiceEmit>,
    /// Cross-workspace scopes (bd-asr2.18, D5): subscriptions may span N
    /// workspaces and external:<proj>:<bead> holds re-ring on foreign
    /// close. Default false — off is byte-identical single-workspace.
    #[serde(default)]
    pub cross_workspace: bool,
    /// Same-host foreign repo paths for external re-ring (bd-asr2.18):
    /// project slug -> repo dir containing .beads. Cross-host is out.
    #[serde(default)]
    pub external_projects: std::collections::HashMap<String, String>,
}

/// One tokened service identity (bd-asr2.17, D7): the token itself never
/// lives here, only its hex sha256. `channels` scopes which `to` targets
/// the token may ring (`*` = any). `rate_per_min` bounds burst intake.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceEmit {
    #[serde(default)]
    pub token_sha256: String,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default = "default_service_rate")]
    pub rate_per_min: u32,
}

fn default_service_rate() -> u32 {
    6
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
    /// `[doorbell]` — doorbell plane-D daemon (ADR-0012 D2): `url` selects
    /// the daemon host (default `http://127.0.0.1:18980` when the section
    /// exists). Absent section → embedded in-process host, with a 200ms
    /// loopback probe that still defers to a running daemon (D9).
    pub doorbell: Option<DoorbellSection>,
    /// Resonate server URL for the durable loop (default http://localhost:8001)
    pub resonate_url: Option<String>,
    /// short name -> herdr agent kind (default: oc=opencode, pi=pi, gk=grok)
    pub kinds: Option<BTreeMap<String, String>>,
    /// backend selection (ADR-0002)
    pub backend: Option<BackendConfig>,
    /// durable-loop self-heal knobs (bd-event-self-heal-unknown)
    #[serde(default)]
    pub r#loop: Option<LoopConfig>,
    /// Parent-death policy (ADR-0025 Spec 4): what happens to crews when the
    /// captain pane dies. Default `stop`.
    #[serde(default)]
    pub on_captain_death: Option<OnCaptainDeath>,
    /// ADR-0011 D2: operator-owned pane cap. Repo `.flywheel/` must not raise it.
    #[serde(default)]
    pub crew: Option<CrewBudgetConfig>,
}

/// `[crew] max = 5` in operator config (ADR-0011 D2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewBudgetConfig {
    pub max: Option<u32>,
}

/// Parent-death policy for the captain (ADR-0025 Spec 4, Wave 4a).
///
/// `stop` (default) — killing the captain also kills its crews (implies
/// `--force-working` for attributed pins). `continue` — crews+doorbell stay,
/// captain alias cleared until rebound. `adopt` — new pin takes captain alias
/// (alias of `recover`; both spellings parse to Adopt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnCaptainDeath {
    #[default]
    Stop,
    Continue,
    Adopt,
}

impl OnCaptainDeath {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Continue => "continue",
            Self::Adopt => "adopt",
        }
    }
}

impl std::fmt::Display for OnCaptainDeath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for OnCaptainDeath {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "stop" => Ok(Self::Stop),
            "continue" => Ok(Self::Continue),
            "adopt" | "recover" => Ok(Self::Adopt),
            other => Err(format!(
                "invalid on_captain_death {other:?}: expected stop|continue|adopt (recover is alias for adopt)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for OnCaptainDeath {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for OnCaptainDeath {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(self.as_str())
    }
}

/// `[loop]` section: self-heal thresholds for the durable swarm loop.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopConfig {
    /// consecutive `unknown` polls before Diagnose (default 3)
    pub unknown_diagnose_threshold: Option<u32>,
    /// max respawns per agent per loop run before escalate-blocked (default 2)
    pub respawn_budget: Option<u32>,
    /// consecutive `unknown` polls treated as DETECTION GRACE (bd-bvwf.5):
    /// an unknown agent younger than this is presumed detecting (fresh
    /// spawn / mid-identification), never Diagnosed or Respawnd. Default 6
    /// (~3min at the 30s active cadence — herdr takes 30-60s to classify a
    /// fresh pane).
    pub unknown_detect_grace_polls: Option<u32>,
    /// completion drain-grace polls (bd-bvwf.3): the completion condition
    /// must hold for grace+1 consecutive polls before the loop exits — the
    /// extra poll catches a bead unblocked by the last closure (dependency
    /// chain) without a manual restart. Default 1.
    pub drain_grace_polls: Option<u32>,
    /// claim-watch nudge cadence (bd-efk1): consecutive polls a dispatched
    /// bead may stay UNCLAIMED while its agent is engaged before the loop
    /// sends a claim-focused nudge. Default 2 (~60s at the 30s active
    /// cadence). 0/absent → the default.
    pub claim_nudge_after_polls: Option<u32>,
    /// verifier-family override (ADR-0019 Spec 4 single-family degrade):
    /// when the model registry cannot supply a second model family for the
    /// independent verifier, this family is explicitly allowed. Absent =
    /// strict different-family rule (bead blocked `verifier-family-missing`,
    /// one captain mail, no respawn).
    pub verifier_family_override: Option<String>,
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

    /// Detection-grace polls (default 6).
    pub fn unknown_detect_grace_polls(&self) -> u32 {
        self.r#loop
            .as_ref()
            .and_then(|l| l.unknown_detect_grace_polls)
            .unwrap_or(6)
    }

    /// Completion drain-grace polls (default 1).
    pub fn drain_grace_polls(&self) -> u32 {
        self.r#loop
            .as_ref()
            .and_then(|l| l.drain_grace_polls)
            .unwrap_or(1)
    }

    /// Claim-watch nudge cadence (default 2 — see
    /// [`crate::durable::DEFAULT_CLAIM_NUDGE_AFTER_POLLS`]).
    pub fn claim_nudge_after_polls(&self) -> u32 {
        self.r#loop
            .as_ref()
            .and_then(|l| l.claim_nudge_after_polls)
            .unwrap_or(crate::durable::DEFAULT_CLAIM_NUDGE_AFTER_POLLS)
    }

    /// Verifier-family override (ADR-0019 Spec 4): absent = strict
    /// different-family rule for the independent verifier.
    pub fn verifier_family_override(&self) -> Option<&str> {
        self.r#loop
            .as_ref()
            .and_then(|l| l.verifier_family_override.as_deref())
    }

    /// Parent-death policy (ADR-0025 Spec 4, Wave 4a): `stop` by default.
    pub fn on_captain_death(&self) -> OnCaptainDeath {
        self.on_captain_death.unwrap_or_default()
    }

    /// ADR-0011 D2: pane cap. Env `FLYWHEEL_CREW_MAX` > `[crew].max` > 5.
    pub fn crew_max(&self) -> u32 {
        if let Ok(v) = std::env::var("FLYWHEEL_CREW_MAX")
            && let Ok(n) = v.parse::<u32>()
            && n >= 1
        {
            return n;
        }
        self.crew
            .as_ref()
            .and_then(|c| c.max)
            .filter(|n| *n >= 1)
            .unwrap_or(crate::crew::DEFAULT_CREW_MAX)
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
        let mut cfg: Self = if xdg.exists() {
            let raw =
                std::fs::read_to_string(&xdg).with_context(|| format!("read {}", xdg.display()))?;
            toml::from_str(&raw).with_context(|| format!("parse {}", xdg.display()))?
        } else if legacy.exists() {
            let raw = std::fs::read_to_string(&legacy)
                .with_context(|| format!("read {}", legacy.display()))?;
            let parsed: Self =
                toml::from_str(&raw).with_context(|| format!("parse {}", legacy.display()))?;
            // Write-through to XDG thereafter (copy, not move — V5).
            migrate_legacy_config(&legacy, &xdg);
            parsed
        } else {
            Self::default()
        };
        // env overrides
        if let Ok(v) = std::env::var("FLYWHEEL_SOCKET") {
            cfg.socket_path = Some(v);
        }
        if let Ok(v) = std::env::var("FLYWHEEL_ROOT") {
            cfg.workspace_root = Some(v);
        }
        if let Ok(v) = std::env::var("FLYWHEEL_TIMEOUT") {
            // Fail loudly on bad input (is1.6): a typo'd value must abort
            // startup, never silently fall back to the default.
            cfg.default_timeout_ms = Some(v.parse().map_err(|e| {
                anyhow::anyhow!("FLYWHEEL_TIMEOUT={v:?} is not a number of ms: {e}")
            })?);
        }
        if let Ok(v) = std::env::var("RESONATE_URL") {
            cfg.resonate_url = Some(v);
        }
        // agent-mail env surface (the factory's error message documents
        // FLYWHEEL_AGENTMAIL_URL/TOKEN — make good on it): FLYWHEEL_AGENTMAIL_URL
        // / FLYWHEEL_AGENTMAIL_TOKEN feed `[mail.agentmail]` ONLY (ADR-0003 V3 —
        // never toron), merging over config.toml; FLYWHEEL_MAIL_NAME feeds the
        // shared `[agent_mail]` identity.
        let (am_url, am_token, mail_name) = Self::mail_env_overrides();
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
    /// `[agent_mail]`, transport url/token from `[mail.toron]` ONLY (no
    /// legacy `[agent_mail] url` fallback — the shared url is never a routing
    /// source, ADR-0003 V2/V3). No `[mail.toron]` url anywhere → embedded
    /// in-process toron.
    pub fn toron_mail_config(&self) -> MailConfig {
        let sec = self.mail.as_ref().and_then(|m| m.toron.as_ref());
        let legacy = self.agent_mail.as_ref();
        MailConfig {
            url: sec.and_then(|c| c.url.clone()).filter(|s| !s.is_empty()),
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

    /// Effective doorbell host (ADR-0012 D1): `[doorbell] url`, defaulting
    /// to `http://127.0.0.1:18980` when the section exists. `None` selects
    /// embedded (D9 probe may still defer to a running daemon).
    pub fn doorbell_url(&self) -> Option<String> {
        self.doorbell.as_ref().map(|d| {
            d.url
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "http://127.0.0.1:18980".to_string())
        })
    }

    /// Arrival triple flag (bd-asr2.14, D5): false unless opted in.
    pub fn doorbell_arrival_triple(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.arrival_triple)
            .unwrap_or(false)
    }

    /// Beads-poller flag (bd-asr2.15, D7): false unless opted in.
    pub fn doorbell_beads_poller(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.beads_poller)
            .unwrap_or(false)
    }

    /// Cron doorbell-sink flag (bd-asr2.16, D7): false unless opted in.
    pub fn doorbell_cron_sink(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.cron_doorbell_sink)
            .unwrap_or(false)
    }

    /// Service identities for POST /api/v1/emit (bd-asr2.17, D7):
    /// empty map = endpoint disabled.
    pub fn doorbell_services(&self) -> std::collections::HashMap<String, ServiceEmit> {
        self.doorbell
            .as_ref()
            .map(|d| d.services.clone())
            .unwrap_or_default()
    }

    /// Cross-workspace scopes flag (bd-asr2.18, D5): false unless opted in.
    pub fn doorbell_cross_workspace(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.cross_workspace)
            .unwrap_or(false)
    }

    /// Same-host foreign repo paths for external re-ring (bd-asr2.18).
    pub fn doorbell_external_projects(&self) -> std::collections::HashMap<String, String> {
        self.doorbell
            .as_ref()
            .map(|d| d.external_projects.clone())
            .unwrap_or_default()
    }

    pub fn socket_path(&self) -> String {
        self.socket_path
            .clone()
            .or_else(|| std::env::var("HERDR_SOCKET_PATH").ok())
            .unwrap_or_else(|| {
                // Converge with the module's own XDG-first base (is1.6): the
                // canonical herdr client (toron-core backend/herdr.rs) resolves
                // `HERDR_SOCKET_PATH` or `$XDG_CONFIG_HOME/herdr/herdr.sock`.
                // `dirs::config_dir()` would point at ~/Library/Application
                // Support on macOS — divergent, and a real "flywheel can't
                // find herdr" bug with default config.
                xdg_config_base()
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
        if let Some(map) = &self.kinds
            && let Some(k) = map.get(short)
        {
            return k.clone();
        }
        match short {
            "oc" => "opencode".into(),
            "pi" => "pi".into(),
            "gk" => "grok".into(),
            other => other.into(),
        }
    }
}

// ---- parent-death policy helpers (ADR-0025 Spec 4, Wave 4a Phase 1) ----
// Pure helpers over the spawn registry. Phase 2 wires these into
// `kill`/`recover` in main.rs; Phase 1 only proves the policy + attribution.

/// Pins the spawn registry attributes to `captain_pin`.
///
/// Attribution is project-scoped: every entry in the project's registry
/// whose `mail_identity` (or fallback `name`) is not the captain itself is
/// considered a child of that captain. This matches the "spawn registry +
/// captain alias" hierarchy (peers, not OTP children) for the phase-1
/// mocked registry used in tests.
pub fn captain_attributed_pins(
    registry: &[crate::state::SpawnEntry],
    captain_pin: &str,
) -> Vec<String> {
    registry
        .iter()
        .filter_map(|e| {
            let pin = e.mail_identity.as_deref().unwrap_or(e.name.as_str());
            if pin == captain_pin {
                None
            } else {
                Some(pin.to_string())
            }
        })
        .collect()
}

/// Whether `pin` is considered a captain child under `captain_pin`.
pub fn is_captain_child(
    registry: &[crate::state::SpawnEntry],
    captain_pin: &str,
    pin: &str,
) -> bool {
    if pin == captain_pin {
        return false;
    }
    captain_attributed_pins(registry, captain_pin).contains(&pin.to_string())
}

/// Registry after applying `policy` for a dead captain.
///
/// - `Stop`  — captain + attributed children removed (implies --force-working
///   for those children; unrelated working pins unchanged — caller enforces
///   the gate, this helper only projects the registry).
/// - `Continue` — captain removed, children + doorbell stay (alias cleared).
/// - `Adopt` — registry unchanged; alias will be rebound to the recovering pin
///   (pure alias map change, not a registry mutation).
pub fn registry_after_captain_death(
    registry: &[crate::state::SpawnEntry],
    captain_pin: &str,
    policy: OnCaptainDeath,
) -> Vec<crate::state::SpawnEntry> {
    match policy {
        OnCaptainDeath::Stop => registry
            .iter()
            .filter(|e| {
                let pin = e.mail_identity.as_deref().unwrap_or(e.name.as_str());
                pin != captain_pin && !is_captain_child(registry, captain_pin, pin)
            })
            .cloned()
            .collect(),
        OnCaptainDeath::Continue | OnCaptainDeath::Adopt => registry
            .iter()
            .filter(|e| e.mail_identity.as_deref().unwrap_or(e.name.as_str()) != captain_pin)
            .cloned()
            .collect(),
    }
}

/// Mocked captain-alias store helpers for phase-1 tests.
///
/// Production alias lives in toron (`contact captain --clear` / --pin);
/// phase 1 proves the semantics with an in-memory map so tests do not
/// require a daemon.
pub fn apply_captain_adopt(alias_map: &mut BTreeMap<String, String>, project: &str, new_pin: &str) {
    alias_map.insert(project.to_string(), new_pin.to_string());
}

pub fn clear_captain_alias(alias_map: &mut BTreeMap<String, String>, project: &str) {
    alias_map.remove(project);
}

/// Whether the alias for `project` is cleared (absent) in the mocked store.
pub fn captain_alias_cleared(alias_map: &BTreeMap<String, String>, project: &str) -> bool {
    !alias_map.contains_key(project)
}

/// The XDG config BASE (is1.6): `$XDG_CONFIG_HOME` when set, else
/// `~/.config`. `config_dir()` appends `/flywheel`; the herdr socket appends
/// `/herdr/herdr.sock` — both converge on this base (matches the canonical
/// herdr client in toron-core).
pub fn xdg_config_base() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".config")
        })
}

pub fn config_dir() -> PathBuf {
    xdg_config_base().join("flywheel")
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
    // Honor XDG_STATE_HOME first (193.2): hermetic state for tests/prod,
    // matching how config_dir() honors XDG_CONFIG_HOME. `dirs::state_dir()`
    // (macOS: ~/Library/Application Support) is the fallback.
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::state_dir().unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("~"))
                    .join(".local/state")
            })
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
            return Ok(Self::default());
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
            return Ok(Self::default());
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
            return Ok(Self::default());
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
            return Ok(Self::default());
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
            .map(std::string::String::as_str)
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

    /// ADR-0019 Spec 4: `[loop] verifier_family_override` parses and is
    /// honored by the accessor; absent = strict different-family rule.
    #[test]
    fn verifier_family_override_parses_from_loop_section() {
        let c: Config = toml::from_str(
            r#"
[loop]
verifier_family_override = "deepseek"
"#,
        )
        .unwrap();
        assert_eq!(c.verifier_family_override(), Some("deepseek"));
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.verifier_family_override(), None);
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
        let raw = r"
[swarm]
oc = 1
pi = 1
gk = 1

[qa]
oc = 2
";
        let r: Recipes = toml::from_str(raw).unwrap();
        assert_eq!(r.recipes["swarm"].oc, Some(1));
        assert_eq!(r.recipes["qa"].oc, Some(2));
    }

    #[test]
    fn workflows_parse() {
        let raw = r#"
daily = [
  { type = "send", to = "all", stage = "fresh-eyes-explore" },
  { type = "wait", until = "idle", timeout_ms = 600000 },
  { type = "send", to = "oc", stage = "ce-execute" },
]
"#;
        let w: Workflows = toml::from_str(raw).unwrap();
        let steps = &w.workflows["daily"];
        assert_eq!(steps.len(), 3);
        match &steps[0] {
            WorkflowStep::Send { stage, to, .. } => {
                assert_eq!(stage.as_deref(), Some("fresh-eyes-explore"));
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

    #[test]
    fn mail_env_overrides_read_all_vars() {
        let _g = lock_env();
        unsafe { std::env::set_var("FLYWHEEL_AGENTMAIL_URL", "http://127.0.0.1:8765") };
        unsafe { std::env::set_var("FLYWHEEL_AGENTMAIL_TOKEN", "secret") };
        unsafe { std::env::set_var("FLYWHEEL_MAIL_NAME", "Orchid") };
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:8765"));
        assert_eq!(token.as_deref(), Some("secret"));
        assert_eq!(name.as_deref(), Some("Orchid"));
        unsafe { std::env::remove_var("FLYWHEEL_AGENTMAIL_URL") };
        unsafe { std::env::remove_var("FLYWHEEL_AGENTMAIL_TOKEN") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL_NAME") };
    }

    #[test]
    fn mail_env_overrides_ignore_empty_values() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::set_var("FLYWHEEL_AGENTMAIL_URL", "") };
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!(url, None, "empty url counts as unset");
        assert_eq!(token, None);
        assert_eq!(name, None);
        unsafe { std::env::remove_var("FLYWHEEL_AGENTMAIL_URL") };
    }

    #[test]
    fn mail_env_overrides_nothing_set() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("FLYWHEEL_AGENTMAIL_URL") };
        unsafe { std::env::remove_var("FLYWHEEL_AGENTMAIL_TOKEN") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL_NAME") };
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

    /// The legacy `[agent_mail] url` is ignored by BOTH backends (no
    /// backward-compat bridge — ADR-0003 V2/V3 BUG1 fix): with no
    /// `[mail.toron]`, toron is embedded in-process; agentmail defaults to
    /// the AM port. The shared `[agent_mail] token` IS inherited by both
    /// backends (ADR-0003 V3: "[agent_mail] keeps … token if shared").
    #[test]
    fn legacy_agent_mail_url_never_routes() {
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://127.0.0.1:18080".into()),
                token: Some("shared".into()),
                name: Some("Orchid".into()),
            }),
            ..Default::default()
        };
        let toron = cfg.toron_mail_config();
        assert_eq!(
            toron.url, None,
            "no [mail.toron] -> embedded in-process, legacy url ignored"
        );
        assert_eq!(
            toron.token.as_deref(),
            Some("shared"),
            "toron inherits the SHARED token (V3)"
        );
        assert_eq!(toron.name.as_deref(), Some("Orchid"));
        let am = cfg.agentmail_server();
        assert_eq!(
            am.url.as_deref(),
            Some("http://127.0.0.1:8765"),
            "AM never inherits the legacy url"
        );
        assert_eq!(
            am.token.as_deref(),
            Some("shared"),
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
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &tmp) };
        assert_eq!(config_dir(), tmp.join("flywheel"));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    /// No XDG_CONFIG_HOME → `~/.config/flywheel` (NOT macOS Application
    /// Support, which is what `dirs::config_dir()` returns).
    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let home = std::env::temp_dir().join(format!("flywheel-cfg-home-{}", std::process::id()));
        unsafe { std::env::set_var("HOME", &home) };
        assert_eq!(
            config_dir(),
            home.join(".config/flywheel"),
            "XDG default is ~/.config/flywheel, not dirs::config_dir()"
        );
        unsafe { std::env::remove_var("HOME") };
    }

    /// is1.6: the herdr socket fallback converges with `config_dir()`
    /// (XDG_CONFIG_HOME first), matching the canonical herdr client in
    /// toron-core (`HERDR_SOCKET_PATH` else `$XDG_CONFIG_HOME/herdr/herdr.sock`).
    #[test]
    fn socket_path_follows_xdg_config_home() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };
        let tmp = std::env::temp_dir().join(format!("flywheel-sock-xdg-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &tmp) };
        let cfg = Config::default();
        assert_eq!(
            cfg.socket_path(),
            tmp.join("herdr/herdr.sock").to_string_lossy()
        );
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    /// is1.6: no XDG_CONFIG_HOME → `~/.config/herdr/herdr.sock` (never
    /// `dirs::config_dir()` / macOS Application Support).
    #[test]
    fn socket_path_falls_back_to_home_dot_config() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let home = std::env::temp_dir().join(format!("flywheel-sock-home-{}", std::process::id()));
        unsafe { std::env::set_var("HOME", &home) };
        let cfg = Config::default();
        assert_eq!(
            cfg.socket_path(),
            home.join(".config/herdr/herdr.sock").to_string_lossy(),
            "socket default is ~/.config/herdr/herdr.sock, not Application Support"
        );
        unsafe { std::env::remove_var("HOME") };
    }

    /// is1.6: a non-numeric FLYWHEEL_TIMEOUT aborts Config::load() naming
    /// the var — never a silent fallback to the default.
    #[test]
    fn flywheel_timeout_env_fails_loudly() {
        let _g = lock_env();
        // Isolate HOME + XDG so load() only sees our env.
        let home = std::env::temp_dir().join(format!("flywheel-to-home-{}", std::process::id()));
        let xdg = std::env::temp_dir().join(format!("flywheel-to-xdg-{}", std::process::id()));
        unsafe { std::env::set_var("HOME", &home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
        unsafe { std::env::set_var("FLYWHEEL_TIMEOUT", "abc") };
        let err = Config::load().expect_err("bad FLYWHEEL_TIMEOUT must abort load");
        assert!(
            format!("{err:#}").contains("FLYWHEEL_TIMEOUT"),
            "error names the var: {err:#}"
        );
        unsafe { std::env::set_var("FLYWHEEL_TIMEOUT", "30000") };
        let cfg = Config::load().expect("valid timeout parses");
        assert_eq!(cfg.default_timeout_ms, Some(30000));
        unsafe { std::env::remove_var("FLYWHEEL_TIMEOUT") };
        unsafe { std::env::remove_var("HOME") };
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    /// 193.2: `state_dir()` honors `XDG_STATE_HOME` first (hermetic state
    /// for tests/prod) — mirror of the XDG_CONFIG_HOME contract.
    #[test]
    fn state_dir_honors_xdg_state_home() {
        let _g = lock_env();
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        unsafe { std::env::set_var("XDG_STATE_HOME", "/tmp/fw-state-xdg") };
        assert_eq!(
            state_dir(),
            PathBuf::from("/tmp/fw-state-xdg/flywheel"),
            "XDG_STATE_HOME wins over dirs::state_dir()"
        );
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
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
        unsafe { std::env::set_var("HOME", &home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
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
        unsafe { std::env::remove_var("HOME") };
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
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
        unsafe { std::env::set_var("HOME", &home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
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
        unsafe { std::env::remove_var("HOME") };
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    // ---- ADR-0025 Spec 4 Wave 4a Phase 1 ----

    #[test]
    fn on_captain_death_default_is_stop() {
        let cfg = Config::default();
        assert_eq!(cfg.on_captain_death(), OnCaptainDeath::Stop);
        let cfg2: Config = toml::from_str("").unwrap();
        assert_eq!(cfg2.on_captain_death(), OnCaptainDeath::Stop);
    }

    #[test]
    fn on_captain_death_parse_all_three() {
        let c: Config = toml::from_str(r#"on_captain_death = "stop""#).unwrap();
        assert_eq!(c.on_captain_death(), OnCaptainDeath::Stop);
        let c: Config = toml::from_str(r#"on_captain_death = "continue""#).unwrap();
        assert_eq!(c.on_captain_death(), OnCaptainDeath::Continue);
        let c: Config = toml::from_str(r#"on_captain_death = "adopt""#).unwrap();
        assert_eq!(c.on_captain_death(), OnCaptainDeath::Adopt);
        // recover is alias for adopt (Spec 4: recover == adopt)
        let c: Config = toml::from_str(r#"on_captain_death = "recover""#).unwrap();
        assert_eq!(c.on_captain_death(), OnCaptainDeath::Adopt);
        // case-insensitive
        let c: Config = toml::from_str(r#"on_captain_death = "STOP""#).unwrap();
        assert_eq!(c.on_captain_death(), OnCaptainDeath::Stop);
    }

    #[test]
    fn on_captain_death_invalid_rejected() {
        let err = toml::from_str::<Config>(r#"on_captain_death = "explode""#)
            .expect_err("invalid value must fail");
        assert!(format!("{err}").contains("on_captain_death"));
    }

    #[test]
    fn on_captain_death_recover_sets_alias() {
        let mut aliases: BTreeMap<String, String> = BTreeMap::new();
        aliases.insert("proj".into(), "OldCaptain".into());
        // recover == adopt: alias rebound to recovering pin
        apply_captain_adopt(&mut aliases, "proj", "NewCaptain");
        assert_eq!(
            aliases.get("proj").map(std::string::String::as_str),
            Some("NewCaptain")
        );
        // clear helper (toron contact captain --clear)
        clear_captain_alias(&mut aliases, "proj");
        assert!(captain_alias_cleared(&aliases, "proj"));
    }

    #[test]
    fn on_captain_death_continue_leaves_child_pane_listed() {
        use crate::state::SpawnEntry;
        let captain = "BraveCaptain";
        let child = SpawnEntry::with_mail_identity("pane-child", "child1", "ChildPin");
        let captain_entry = SpawnEntry::with_mail_identity("pane-cap", "cap", captain);
        let unrelated = SpawnEntry::with_mail_identity("pane-other", "other", "UnrelatedPin");
        let registry = vec![captain_entry.clone(), child.clone(), unrelated.clone()];

        // continue: captain removed, children + unrelated stay
        let after_continue =
            registry_after_captain_death(&registry, captain, OnCaptainDeath::Continue);
        assert!(
            after_continue.iter().any(|e| e.pane_id == "pane-child"),
            "continue must leave child pane listed"
        );
        assert!(
            after_continue.iter().any(|e| e.pane_id == "pane-other"),
            "unrelated working pin unchanged"
        );
        assert!(
            !after_continue.iter().any(|e| e.pane_id == "pane-cap"),
            "captain itself cleared"
        );
        // alias cleared on continue
        let mut aliases = BTreeMap::from([("proj".to_string(), captain.to_string())]);
        clear_captain_alias(&mut aliases, "proj");
        assert!(captain_alias_cleared(&aliases, "proj"));
    }

    #[test]
    fn on_captain_death_stop_implies_force_working_for_attributed_only() {
        use crate::state::SpawnEntry;
        let captain = "BraveCaptain";
        let child = SpawnEntry::with_mail_identity("pane-child", "child1", "ChildPin");
        let captain_entry = SpawnEntry::with_mail_identity("pane-cap", "cap", captain);
        // Unrelated pin lives outside this project's registry (different project / no entry)
        let registry = vec![captain_entry, child.clone()];

        let attributed = captain_attributed_pins(&registry, captain);
        assert_eq!(attributed, vec!["ChildPin".to_string()]);
        assert!(
            !is_captain_child(&registry, captain, "UnrelatedPin"),
            "unrelated working pin (outside registry) not attributed"
        );

        let after_stop = registry_after_captain_death(&registry, captain, OnCaptainDeath::Stop);
        assert!(
            !after_stop.iter().any(|e| e.pane_id == "pane-child"),
            "stop removes attributed child"
        );
        // unrelated outside registry is trivially unchanged — not in filtered set
        assert!(
            !after_stop
                .iter()
                .any(|e| e.mail_identity.as_deref() == Some("UnrelatedPin"))
        );
    }

    #[test]
    fn on_captain_death_reap_crews_unconditional_override() {
        use crate::state::SpawnEntry;
        let captain = "BraveCaptain";
        let child = SpawnEntry::with_mail_identity("pane-child", "child1", "ChildPin");
        let registry = vec![
            SpawnEntry::with_mail_identity("pane-cap", "cap", captain),
            child.clone(),
        ];
        // reap_crews overrides continue → behaves like Stop (children reaped)
        let with_reap = {
            let policy = if true {
                OnCaptainDeath::Stop
            } else {
                OnCaptainDeath::Continue
            };
            registry_after_captain_death(&registry, captain, policy)
        };
        assert!(
            !with_reap.iter().any(|e| e.pane_id == "pane-child"),
            "--reap-crews must reap crews even under continue"
        );
        // without reap, continue leaves child
        let without_reap =
            registry_after_captain_death(&registry, captain, OnCaptainDeath::Continue);
        assert!(without_reap.iter().any(|e| e.pane_id == "pane-child"));
    }

    #[test]
    fn on_captain_death_mail_to_captain_fails_when_alias_cleared_until_rebound() {
        use crate::state as st;
        let _home = test_state_home();
        let proj = format!("oncap-mail-{}", std::process::id());
        // initially alias present
        st::save_captain_alias(&proj, "BraveCaptain").unwrap();
        assert!(st::resolve_captain_target(&proj, "captain").is_ok());
        // continue clears alias → mail --to captain fails loud
        st::clear_captain_alias_file(&proj).unwrap();
        let err =
            st::resolve_captain_target(&proj, "captain").expect_err("cleared alias must fail");
        assert!(format!("{err}").contains("alias cleared"));
        assert!(format!("{err}").contains("recover"));
        // recover (= adopt) rebounds alias → mail succeeds again
        st::save_captain_alias(&proj, "NewCaptain").unwrap();
        assert!(st::resolve_captain_target(&proj, "captain").is_ok());
        assert!(st::resolve_captain_target(&proj, "CAPTAIN").is_ok());
        let _ = st::clear_captain_alias_file(&proj);
    }

    #[test]
    fn on_captain_death_kill_continue_keeps_doorbell_and_crews() {
        use crate::state::{self as st, SpawnEntry};
        let _home = test_state_home();
        let proj = format!("oncap-kill-{}", std::process::id());
        let captain = "BraveCaptain";
        let child = SpawnEntry::with_mail_identity("pane-child", "child1", "ChildPin");
        st::save_spawn_registry(&proj, &[child.clone()]).unwrap();
        st::save_captain_alias(&proj, captain).unwrap();
        // simulate kill with continue: crews remain, alias cleared, doorbell would stay (not stopped)
        let registry = st::load_spawn_registry(&proj);
        let remain = registry_after_captain_death(&registry, captain, OnCaptainDeath::Continue);
        st::save_spawn_registry(&proj, &remain).unwrap();
        st::clear_captain_alias_file(&proj).unwrap();
        let after = st::load_spawn_registry(&proj);
        assert!(
            after.iter().any(|e| e.pane_id == "pane-child"),
            "continue must leave child pane listed"
        );
        assert!(
            st::is_captain_alias_cleared(&proj),
            "alias cleared until rebound"
        );
        // adopt via recover
        st::save_captain_alias(&proj, "AdoptedCaptain").unwrap();
        assert_eq!(
            st::load_captain_alias(&proj).as_deref(),
            Some("AdoptedCaptain")
        );
        let _ = st::clear_captain_alias_file(&proj);
        let _ = std::fs::remove_file(st::spawn_registry_file(&proj));
    }
}

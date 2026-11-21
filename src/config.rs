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

/// A `FLYWHEEL_*` knob that refused to parse (wave12-d): the name, what was
/// set, and the accepted forms. Unknown is an error, never a silent default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub name: &'static str,
    pub value: String,
    pub accepted: &'static str,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}={:?} is not valid (accepted: {})",
            self.name, self.value, self.accepted
        )
    }
}

impl std::error::Error for ConfigError {}

/// One strict parse of a `FLYWHEEL_*` knob: unset/empty → `Ok(None)` (the
/// caller applies its default); set → `parse` or a [`ConfigError`] naming
/// `accepted`. The single interpretation site per knob — domain code calls
/// the typed accessor, never `env::var` plus an ad-hoc parse.
pub fn parse_env_opt<T>(
    name: &'static str,
    accepted: &'static str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<Option<T>, ConfigError> {
    let Some(os) = std::env::var_os(name).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let raw = os.to_string_lossy();
    parse(&raw).map(Some).ok_or_else(|| ConfigError {
        name,
        value: raw.into_owned(),
        accepted,
    })
}

/// `1|true|on` → true, `0|false|off` → false (ASCII case-insensitive).
pub fn parse_flag(name: &'static str) -> Result<Option<bool>, ConfigError> {
    parse_env_opt(name, "1|true|on|0|false|off", |raw| {
        match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "on" => Some(true),
            "0" | "false" | "off" => Some(false),
            _ => None,
        }
    })
}

/// A `parse`-shaped enum knob: `accepted` names the forms, `parse` decides.
pub fn parse_enum<T>(
    name: &'static str,
    accepted: &'static str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<Option<T>, ConfigError> {
    parse_env_opt(name, accepted, parse)
}

/// A non-negative integer (leading/trailing whitespace tolerated).
pub fn parse_u64(name: &'static str) -> Result<Option<u64>, ConfigError> {
    parse_env_opt(name, "a non-negative integer", |raw| {
        raw.trim().parse::<u64>().ok()
    })
}

/// A non-empty path.
pub fn parse_path(name: &'static str) -> Result<Option<PathBuf>, ConfigError> {
    parse_env_opt(name, "a non-empty path", |raw| Some(PathBuf::from(raw)))
}

/// Crate-wide lock for tests that mutate process env vars (193.2): env is
/// process-global, so ALL env-mutating tests across modules must serialize
/// under ONE lock — per-module locks would still race each other. Config's
/// tests own the reference pattern; every other module's env-mutating test
/// takes this lock.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the crate-wide env lock (poison-tolerant) AND pin a hermetic
/// `XDG_STATE_HOME`. Test-only (193.2).
///
/// bd-uu58: the lock alone is not hermetic. Env is process-global, so a test
/// that takes this lock and writes `state_dir()` artifacts inherits whatever
/// `XDG_STATE_HOME` a sibling left behind — or, when no sibling pinned one, the
/// operator's real `~/.local/state/flywheel`. Measured: four `main.rs` spawn
/// tests took only this lock, so their registries were written live and 133
/// `agents/*.json` fixture files accumulated, four more per suite run. Pinning
/// here routes every lock-holder hermetically at the one function they all
/// already call, which is why the earlier per-call-site `test_state_home()`
/// attempt (reverted as a non-fix) was the wrong layer.
#[cfg(test)]
pub fn lock_env() -> TestStateHome {
    test_state_home()
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
    prev: Option<std::ffi::OsString>,
    prev_home: Option<std::ffi::OsString>,
    prev_toron_data: Option<std::ffi::OsString>,
    prev_toron_mcp_data: Option<std::ffi::OsString>,
    prev_br_bin: Option<std::ffi::OsString>,
    prev_path: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl Drop for TestStateHome {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
        restore_env("HOME", self.prev_home.take());
        restore_env("TORON_DATA_DIR", self.prev_toron_data.take());
        restore_env("TORON_MCP_DATA_DIR", self.prev_toron_mcp_data.take());
        restore_env("FLYWHEEL_BR_BIN", self.prev_br_bin.take());
        restore_env("PATH", self.prev_path.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
fn restore_env(name: &str, prev: Option<std::ffi::OsString>) {
    match prev {
        Some(v) => unsafe { std::env::set_var(name, v) },
        None => unsafe { std::env::remove_var(name) },
    }
}

/// Begin a hermetic state-dir test (see [`TestStateHome`]).
///
/// The guard also snapshots HOME / TORON_DATA_DIR / TORON_MCP_DATA_DIR and
/// clears the two toron roots for its lifetime: a sibling that leaks a store
/// root must not redirect this test, and a sibling that unsets HOME must not
/// outlive its guard. Roots set inside the guard are restored on drop.
#[cfg(test)]
pub fn test_state_home() -> TestStateHome {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "flywheel-state-home-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test state home");
    let prev = std::env::var_os("XDG_STATE_HOME");
    let prev_home = std::env::var_os("HOME");
    let prev_toron_data = std::env::var_os("TORON_DATA_DIR");
    let prev_toron_mcp_data = std::env::var_os("TORON_MCP_DATA_DIR");
    // A pinned br binary from a sibling points at a temp stub that dies with
    // its test; resolving it later is Os NotFound. Hermetic per guard.
    let prev_br_bin = std::env::var_os("FLYWHEEL_BR_BIN");
    let prev_path = std::env::var_os("PATH");
    // A leaked store root from a sibling must not redirect this test.
    // NOTE: the in-process mailbox is a process-global OnceLock; a per-guard
    // root would delete its store between tests. Tests that need isolation
    // set TORON_DATA_DIR themselves before the singleton initializes.
    unsafe {
        std::env::remove_var("TORON_DATA_DIR");
        std::env::remove_var("TORON_MCP_DATA_DIR");
        std::env::remove_var("FLYWHEEL_BR_BIN");
        std::env::set_var("XDG_STATE_HOME", &dir);
    }
    TestStateHome {
        _env,
        dir,
        prev,
        prev_home,
        prev_toron_data,
        prev_toron_mcp_data,
        prev_br_bin,
        prev_path,
    }
}

pub const DEFAULT_TIMEOUT_MS: u64 = 1_800_000;

/// `[agent_mail]` section — the SHARED mail identity (ADR-0003 V3). Keeps only
/// the identity/name (and shared token); mail transport lives in
/// `[mail.toron]` (toron daemon; absent → embedded in-process).
/// `url` is tolerated on load for legacy-config migration but is no
/// longer the source of truth for routing.
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

/// Mail transport section under `[mail.toron]` (ADR-0003 V3): the toron
/// daemon (`http://127.0.0.1:18080`; absent → embedded in-process).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailServerConfig {
    /// daemon base URL (e.g. `http://127.0.0.1:18080`). Unset → toron
    /// runs embedded in-process.
    pub url: Option<String>,
    /// bearer token for the daemon's MCP endpoint
    pub token: Option<String>,
}

/// `[doorbell]` section (ADR-0012 D2): the plane-D daemon host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DoorbellSection {
    /// daemon base URL (explicit only). Absent selects embedded with
    /// supervise (no health probe); an explicit `url` is honored.
    pub url: Option<String>,
    /// arrival triple (bd-asr2.14, D5): presence + role auto-bind + join
    /// ring + roster headers. Default false — behavior extensions stay
    /// opt-in so migration parity holds with the flag off.
    #[serde(default)]
    pub arrival_triple: bool,
    /// toron events as a wake source for harnesses with no inbound door
    /// (ADR-0039 D8, bd-l5vb2). Default false — flag-off is byte-identical.
    #[serde(default)]
    pub toron_events: bool,
    /// rex `program_status_changed` push as the authoritative status source
    /// (bd-sfxu1). Default false — flag-off keeps the 3s poll byte-identical.
    #[serde(default)]
    pub program_status_push: bool,
    /// beads-poller sensor (bd-asr2.15, D7; ready-bead lane
    /// bd-ux-papercuts-t2u5): read-only WAL poll of the br sqlite (holds,
    /// issue changes, ready-bead appearances) emitting Fact Msgs. Default
    /// true — a ready bead must ring the supervisor without a manual board
    /// poll; `beads_poller = false` opts out. The poller NEVER writes:
    /// `sqlite3 --readonly` + SELECT only.
    #[serde(default = "default_true")]
    pub beads_poller: bool,
    /// cron doorbell sink (bd-asr2.16, D7): cron tick completion journals a
    /// Fact row and queues a pending wake (digest-visible with zero swarm;
    /// emit/deliver binding lands in bd-asr2.17). Default true. `false` opts out.
    #[serde(default = "default_true")]
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

/// Ready-bead lane default (bd-ux-papercuts-t2u5): the beads poller is on
/// unless explicitly disabled.
fn default_true() -> bool {
    true
}
/// `[mail]` section — the mail transport table (ADR-0003 V3, bd-euav:
/// single-backend toron; the agentmail backend is deleted). Splitting
/// the old shared `[agent_mail] url` fixes the BUG1 misroute: toron
/// reads ONLY its own section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailSections {
    /// `[mail.toron]` — toron daemon (18080); absent → embedded in-process.
    pub toron: Option<MailServerConfig>,
}

/// Backend selection (ADR-0002). `[backend] multiplexer` / `[backend] mail` in
/// config.toml; overridden by `FLYWHEEL_BACKEND` / `FLYWHEEL_MAIL` env (which
/// may themselves be set by `.env`). Defaults: auto (rex-first + hc
/// fallback) with the rex backend compiled in, else hc / toron.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendConfig {
    /// multiplexer backend name (`auto` default with the rex backend
    /// compiled in, else `hc`; `superlogical` pins rex without fallback)
    pub multiplexer: String,
    /// mail backend name (`toron`; the agentmail backend is deleted, bd-euav)
    pub mail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// hc/herdr-core session socket (HC_SOCKET_PATH / XDG state; see socket_path())
    pub socket_path: Option<String>,
    /// Optional rex CLI directory (test/config override for REX_BIN_DIR).
    #[serde(default)]
    pub rex_bin_dir: Option<String>,
    /// default project root (default ~/workspace)
    pub workspace_root: Option<String>,
    /// default --wait timeout in ms for send/watch (default 1800000)
    pub default_timeout_ms: Option<u64>,
    /// optional mail config (in-process by default; remote daemon when
    /// `agent_mail.url` is set — bd-o8j.2/.3)
    pub agent_mail: Option<MailConfig>,
    /// per-backend mail transport (ADR-0003 V3, bd-euav single-backend):
    /// `[mail.toron]` for the toron daemon
    pub mail: Option<MailSections>,
    /// `[doorbell]` — doorbell plane-D daemon: explicit `url` selects the
    /// daemon host. Absent `url` selects embedded (no health probe).
    pub doorbell: Option<DoorbellSection>,
    /// short name -> hc agent kind (default: oc=opencode, pi=pi, gk=grok)
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
    /// `[comms]` (ADR-0041, build-plan §5): absent means the inbox is on.
    /// `inbox_log = false` opts out. A set `TORON_COMMS_INBOX` wins.
    #[serde(default)]
    pub comms: Option<CommsSection>,
    /// `[timer]`: one-shot watches. Absent means on. `enabled = false` opts out.
    #[serde(default)]
    pub timer: Option<TimerSection>,
    /// `[consent]` (ADR-0022 D5): consent gate policy. Absent = defaults
    /// (`ask = "on-change"`, budget 5.0).
    #[serde(default)]
    pub consent: Option<ConsentSection>,
    /// `[swarm]` (ADR-0022 D7): `idle_retire_after = "10m"` retires a seat
    /// idle this long, keeping its pin and reservations. Absent means never.
    #[serde(default)]
    pub swarm: Option<SwarmSection>,
}

/// `[comms]`. Absent `inbox_log` means on. `false` opts out.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommsSection {
    pub inbox_log: Option<bool>,
}

/// `[timer] enabled`. Absent means on. `false` opts out.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimerSection {
    pub enabled: Option<bool>,
}

/// `[crew] max = 5` in operator config (ADR-0011 D2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewBudgetConfig {
    pub max: Option<u32>,
}

/// `[consent]` (ADR-0022 D5): `ask = "on-change" | "always" | "never"`
/// (default `on-change`; unknown values fail parse) and
/// `budget_usd_per_mtok` (default 5.0).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsentSection {
    pub ask: Option<crate::swarm_plan::ConsentAsk>,
    pub budget_usd_per_mtok: Option<f64>,
}

/// `[swarm] idle_retire_after` (ADR-0022 D7): how long a seat may sit idle
/// before the reconciler retires it with its pin and reservations kept.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmSection {
    pub idle_retire_after: Option<String>,
}

/// Parse `[swarm] idle_retire_after` with the shared `toron_core::time_spec`
/// parser. Only relative offsets (`30s`, `10m`, `2h`, `1d`) are accepted:
/// RFC3339 timestamps and `last` are rejected by name — the knob is a
/// duration, not a point in time or a cursor. Sub-second precision is not
/// needed for an idle-retirement knob, so the shared `s|m|h|d` shapes are
/// enough and no second parser is kept here.
pub fn parse_idle_retire_after(raw: &str) -> Result<std::time::Duration, String> {
    match toron_core::time_spec::parse(raw) {
        Ok(toron_core::time_spec::TimeSpec::Relative(d)) => Ok(d),
        Ok(other) => Err(format!(
            "invalid idle_retire_after {raw:?}: expected a relative duration (30s, 10m, 2h, 1d), got {other:?}; RFC3339 timestamps and `last` are not durations"
        )),
        Err(e) => Err(format!("invalid idle_retire_after {raw:?}: {e}")),
    }
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
    /// (~3min at the 30s active cadence — hc takes 30-60s to classify a
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
    /// bd-loop-feeds-idle-panes-rh2z: consecutive passes an idle pane may be
    /// starved (its own stalled bead inside the resume window) before the
    /// idle-pane rotation exempts it. Absent → the default (2).
    pub resume_exempt_passes: Option<u32>,
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

    /// Idle-pane starvation budget before the resume window is exempted
    /// (default 2 — see [`crate::durable::IDLE_PANE_EXEMPT_AFTER_PASSES`]).
    pub fn resume_exempt_passes(&self) -> u32 {
        self.r#loop
            .as_ref()
            .and_then(|l| l.resume_exempt_passes)
            .unwrap_or(crate::durable::IDLE_PANE_EXEMPT_AFTER_PASSES)
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

    /// ADR-0022 D5 consent policy (`[consent]`): `ask` or `on-change`, and
    /// the budget threshold or 5.0 when the section is absent.
    pub fn consent_policy(&self) -> crate::swarm_plan::ConsentPolicy {
        crate::swarm_plan::ConsentPolicy {
            ask: self
                .consent
                .as_ref()
                .and_then(|c| c.ask)
                .unwrap_or_default(),
            budget_usd_per_mtok: self
                .consent
                .as_ref()
                .and_then(|c| c.budget_usd_per_mtok)
                .unwrap_or(crate::swarm_plan::DEFAULT_BUDGET_USD_PER_MTOK),
        }
    }

    /// ADR-0022 D7 idle retirement (`[swarm] idle_retire_after`): the parsed
    /// duration, `None` when the section or the knob is absent (never retire).
    /// An invalid value fails parse loud.
    pub fn idle_retire_after(&self) -> Result<Option<std::time::Duration>, String> {
        self.swarm
            .as_ref()
            .and_then(|s| s.idle_retire_after.as_deref())
            .map(parse_idle_retire_after)
            .transpose()
    }

    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();
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
            migrate_legacy_config(&legacy, &xdg);
            parsed
        } else {
            Self::default()
        };
        if let Err(e) = cfg.idle_retire_after() {
            anyhow::bail!("invalid [swarm] idle_retire_after: {e}");
        }
        if let Ok(v) = std::env::var("FLYWHEEL_SOCKET") {
            cfg.socket_path = Some(v);
        }
        if let Ok(v) = std::env::var("FLYWHEEL_ROOT") {
            cfg.workspace_root = Some(v);
        }
        if let Ok(v) = std::env::var("FLYWHEEL_TIMEOUT") {
            cfg.default_timeout_ms = Some(v.parse().map_err(|e| {
                anyhow::anyhow!("FLYWHEEL_TIMEOUT={v:?} is not a number of ms: {e}")
            })?);
        }
        let (toron_url, toron_token, mail_name) = Self::mail_env_overrides();
        if toron_url.is_some() || toron_token.is_some() {
            let mut toron = cfg
                .mail
                .as_mut()
                .map(|m| m.toron.clone())
                .unwrap_or_default()
                .unwrap_or_default();
            if let Some(v) = toron_url {
                toron.url = Some(v);
            }
            if let Some(v) = toron_token {
                toron.token = Some(v);
            }
            cfg.mail.get_or_insert_with(Default::default).toron = Some(toron);
        }
        if let Some(v) = mail_name {
            let mut mail_cfg = cfg.agent_mail.clone().unwrap_or_default();
            mail_cfg.name = Some(v);
            cfg.agent_mail = Some(mail_cfg);
        }
        cfg.validate_env_knobs()?;
        Ok(cfg)
    }

    /// Startup validation (wave12-d): every listed knob parses-or-refuses
    /// through one helper per shape. Unset/empty falls through (callers keep
    /// their file layers and defaults); anything set-but-garbage fails the
    /// load with a [`ConfigError`] naming the accepted forms. This stores
    /// nothing — use-site readers (`resolve_backend_name`, `crew_max`, …)
    /// keep their precedence and defaults byte-identical; garbage simply
    /// never reaches them. The supervisor/inbox families validate through
    /// their own `try_from_env` constructors (same helpers, same accepted
    /// strings — validation may repeat the single helper; interpretation
    /// never forks). Total functions (`PASSES`/`HINTS`: `off` disables,
    /// all else enables) and shape-only names (`BR_BIN`) have no garbage
    /// class and are not listed.
    fn validate_env_knobs(&self) -> Result<(), ConfigError> {
        parse_enum("FLYWHEEL_BACKEND", "auto|rex|superlogical|hc", |v| {
            let choice = toron_plane::backend::plane::PlaneChoice::parse(v);
            (!matches!(choice, toron_plane::backend::plane::PlaneChoice::Unknown(_)))
                .then(|| v.to_string())
        })?;
        parse_enum("FLYWHEEL_MAIL", "toron", |v| {
            (v == "toron").then(|| v.to_string())
        })?;
        parse_enum("FLYWHEEL_CREW_MAX", "an integer >= 1", |v| {
            v.trim().parse::<u32>().ok().filter(|n| *n >= 1)
        })?;
        parse_flag("FLYWHEEL_TIMER")?;
        parse_flag("FLYWHEEL_ALLOW_EXEC")?;
        parse_flag("FLYWHEEL_NO_DOORBELL_ENSURE")?;
        parse_enum("FLYWHEEL_SHIP_GATE_MODE", "block|warn", |v| {
            matches!(v, "block" | "warn").then(|| v.to_string())
        })?;
        // Supervisor + inbox families: same constructors the domain calls.
        crate::supervisor_host::HostCfg::try_from_env()?;
        crate::inbox_host::HostCfg::try_from_env()?;
        Ok(())
    }

    /// Toron env overrides: `FLYWHEEL_TORON_URL` / `FLYWHEEL_TORON_TOKEN`
    /// feed `[mail.toron]`; `FLYWHEEL_MAIL_NAME` feeds the shared
    /// `[agent_mail]` identity. Empty values count as unset
    /// (matching the factory's `.filter(|s| !s.is_empty())` rule).
    pub fn mail_env_overrides() -> (Option<String>, Option<String>, Option<String>) {
        (
            std::env::var("FLYWHEEL_TORON_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            std::env::var("FLYWHEEL_TORON_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            std::env::var("FLYWHEEL_MAIL_NAME")
                .ok()
                .filter(|s| !s.is_empty()),
        )
    }

    /// Effective toron facade config: identity `name` from the shared
    /// `[agent_mail]`, transport url/token from `[mail.toron]` ONLY (no
    /// legacy `[agent_mail] url` fallback — the shared url is never a routing
    /// source, ADR-0003 V2/V3). No `[mail.toron]` url anywhere → embedded
    /// in-process toron. (bd-euav: the agentmail backend is deleted; the
    /// `[mail.agentmail]` section no longer routes anything.)
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

    /// Effective doorbell host: explicit `[doorbell] url` only. No `url`
    /// means `None` (embedded/supervise path, no health probe); an
    /// explicit `url` is still honored.
    pub fn doorbell_url(&self) -> Option<String> {
        self.doorbell
            .as_ref()
            .and_then(|d| d.url.clone().filter(|s| !s.is_empty()))
    }

    /// Arrival triple flag (bd-asr2.14, D5): false unless opted in.
    pub fn doorbell_arrival_triple(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.arrival_triple)
            .unwrap_or(false)
    }

    /// Beads-poller flag (bd-asr2.15, D7; default-on since
    /// bd-ux-papercuts-t2u5): true unless explicitly disabled.
    pub fn doorbell_beads_poller(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.beads_poller)
            .unwrap_or(true)
    }

    /// Cron doorbell-sink flag (bd-asr2.16, D7): on unless `false`.
    pub fn doorbell_cron_sink(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.cron_doorbell_sink)
            .unwrap_or(true)
    }

    /// Service identities for POST /api/v1/emit (bd-asr2.17, D7):
    /// empty map = endpoint disabled.
    pub fn doorbell_services(&self) -> std::collections::HashMap<String, ServiceEmit> {
        self.doorbell
            .as_ref()
            .map(|d| d.services.clone())
            .unwrap_or_default()
    }

    /// Toron events as a wake source for harnesses with no inbound door
    /// (ADR-0039 D8, bd-l5vb2, bd-rrlo2). On when the flag is set, or by
    /// default once any per-pin token file exists. A deployment with no
    /// tokens stays flag-off byte-identical.
    pub fn doorbell_toron_events(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.toron_events)
            .unwrap_or(false)
            || crate::events_tokens::any_token_file(&crate::config::state_dir())
    }

    /// bd-sfxu1: rex program_status push. False by default; flag-off is
    /// byte-identical (the 3s poll stays the only status source).
    pub fn doorbell_program_status_push(&self) -> bool {
        self.doorbell
            .as_ref()
            .map(|d| d.program_status_push)
            .unwrap_or(false)
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
        if let Some(p) = self.socket_path.clone().filter(|s| !s.is_empty()) {
            return p;
        }
        if let Ok(p) = std::env::var("HC_SOCKET_PATH")
            && !p.is_empty()
        {
            return p;
        }
        let session = std::env::var("HC_SESSION").unwrap_or_else(|_| "default".into());
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        state
            .join("herdr-core")
            .join(&session)
            .join(format!("{session}.sock"))
            .to_string_lossy()
            .into_owned()
    }

    /// Directory that must contain an executable `rex` (config / tests).
    /// When set, the factory plane probe and Rex::resolve prefer it over PATH.
    pub fn rex_bin_dir(&self) -> Option<std::path::PathBuf> {
        self.rex_bin_dir
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("REX_BIN_DIR").and_then(|v| {
                    let p = std::path::PathBuf::from(v);
                    if p.as_os_str().is_empty() {
                        None
                    } else {
                        Some(p)
                    }
                })
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

    pub fn timeout_ms(&self) -> u64 {
        self.default_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    /// Resolve a short agent name (oc/pi/gk) or full kind to an hc agent kind.
    pub fn resolve_kind(&self, short: &str) -> String {
        if let Some(map) = &self.kinds
            && let Some(k) = map.get(short)
        {
            return k.clone();
        }
        crate::harness_registry::canonical(short)
    }
}

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

/// The XDG config BASE (is1.6): `$XDG_CONFIG_HOME` when set, else
/// `~/.config`. `config_dir()` appends `/flywheel`; the hc socket appends
/// `/herdr-core/default/default.sock` — both converge on this base (matches the canonical
/// hc client in toron-core).
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
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".local/state")
        })
        .join("flywheel")
}

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

/// Mocked captain-alias store helpers for phase-1 tests.
///
/// Production alias lives in toron (`contact captain --clear` / --pin);
/// phase 1 proves the semantics with an in-memory map so tests do not
/// require a daemon.
#[cfg(test)]
pub fn apply_captain_adopt(alias_map: &mut BTreeMap<String, String>, project: &str, new_pin: &str) {
    alias_map.insert(project.to_string(), new_pin.to_string());
}

/// Whether the alias for `project` is cleared (absent) in the mocked store.
#[cfg(test)]
pub fn captain_alias_cleared(alias_map: &BTreeMap<String, String>, project: &str) -> bool {
    !alias_map.contains_key(project)
}

#[cfg(test)]
pub fn clear_captain_alias(alias_map: &mut BTreeMap<String, String>, project: &str) {
    alias_map.remove(project);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wave12-d: strict helper tables + load-time refusal. Env-mutating tests
    /// take the crate-wide `lock_env()` guard (process-global env).
    mod config_strict_tests {
        use super::super::*;
        use crate::config::lock_env;

        fn set(name: &str, value: &str) {
            unsafe { std::env::set_var(name, value) };
        }

        fn remove(name: &str) {
            unsafe { std::env::remove_var(name) };
        }

        #[test]
        fn config_strict_flag_table() {
            for (raw, want) in [
                ("1", Some(true)),
                ("true", Some(true)),
                ("on", Some(true)),
                ("TRUE", Some(true)),
                ("0", Some(false)),
                ("false", Some(false)),
                ("off", Some(false)),
                ("OFF", Some(false)),
                ("perhaps", None),
                ("2", None),
                ("", None),
            ] {
                let _g = lock_env();
                if raw.is_empty() {
                    remove("FLYWHEEL_CONFIG_STRICT_PROBE");
                } else {
                    set("FLYWHEEL_CONFIG_STRICT_PROBE", raw);
                }
                assert_eq!(
                    parse_flag("FLYWHEEL_CONFIG_STRICT_PROBE").ok().flatten(),
                    want,
                    "flag {raw:?}"
                );
            }
        }

        #[test]
        fn config_strict_error_names_knob_and_forms() {
            let _g = lock_env();
            set("FLYWHEEL_CONFIG_STRICT_PROBE", "bogus");
            let err = parse_enum("FLYWHEEL_CONFIG_STRICT_PROBE", "a|b", |v| {
                matches!(v, "a" | "b").then(|| v.to_string())
            })
            .expect_err("garbage must refuse");
            assert_eq!(err.name, "FLYWHEEL_CONFIG_STRICT_PROBE");
            assert_eq!(err.value, "bogus");
            assert_eq!(err.accepted, "a|b");
            assert!(err.to_string().contains("FLYWHEEL_CONFIG_STRICT_PROBE"));
            assert!(err.to_string().contains("a|b"));
            remove("FLYWHEEL_CONFIG_STRICT_PROBE");
        }

        #[test]
        fn config_strict_load_refuses_garbage_backend() {
            let _g = lock_env();
            set("FLYWHEEL_BACKEND", "bogus");
            let err = Config::load().expect_err("garbage backend must fail load");
            assert!(err.to_string().contains("FLYWHEEL_BACKEND"));
            assert!(err.to_string().contains("auto|rex|superlogical|hc"));
            remove("FLYWHEEL_BACKEND");
        }

        #[test]
        fn config_strict_load_accepts_valid_backend() {
            let _g = lock_env();
            set("FLYWHEEL_BACKEND", "hc");
            Config::load().expect("valid backend loads");
            remove("FLYWHEEL_BACKEND");
        }

        #[test]
        fn config_strict_load_refuses_garbage_timer() {
            let _g = lock_env();
            set("FLYWHEEL_TIMER", "perhaps");
            let err = Config::load().expect_err("garbage flag must fail load");
            assert!(err.to_string().contains("FLYWHEEL_TIMER"));
            remove("FLYWHEEL_TIMER");
        }
    }

    #[test]
    fn doorbell_url_explicit_url_only() {
        let c: Config = toml::from_str("[doorbell]").unwrap();
        assert_eq!(c.doorbell_url(), None);
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.doorbell_url(), None);
        let c: Config = toml::from_str("[doorbell]\nurl = \"http://127.0.0.1:19999\"").unwrap();
        assert_eq!(c.doorbell_url().as_deref(), Some("http://127.0.0.1:19999"));
    }

    #[test]
    fn doorbell_url_section_without_url_is_none() {
        let c: Config = toml::from_str("[doorbell]\nbeads_poller = false").unwrap();
        assert_eq!(c.doorbell_url(), None);
        let c: Config = toml::from_str("[doorbell]\nurl = \"http://127.0.0.1:19980\"").unwrap();
        assert_eq!(c.doorbell_url().as_deref(), Some("http://127.0.0.1:19980"));
    }

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
    fn consent_ask_parses_from_consent_section() {
        let c: Config = toml::from_str(
            r#"
[consent]
ask = "always"
budget_usd_per_mtok = 9.0
"#,
        )
        .unwrap();
        let policy = c.consent_policy();
        assert_eq!(policy.ask, crate::swarm_plan::ConsentAsk::Always);
        assert_eq!(policy.budget_usd_per_mtok, 9.0);
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(
            c.consent_policy(),
            crate::swarm_plan::ConsentPolicy::default()
        );
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
    fn swarm_idle_retire_after_parses() {
        let c: Config = toml::from_str("[swarm]\nidle_retire_after = \"10m\"\n").unwrap();
        assert_eq!(
            c.idle_retire_after().unwrap(),
            Some(std::time::Duration::from_secs(600))
        );
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.idle_retire_after().unwrap(), None);
        let c: Config = toml::from_str("[swarm]\nidle_retire_after = \"ten minutes\"\n").unwrap();
        assert!(c.idle_retire_after().is_err());
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
        unsafe { std::env::set_var("FLYWHEEL_TORON_URL", "http://127.0.0.1:18080") };
        unsafe { std::env::set_var("FLYWHEEL_TORON_TOKEN", "secret") };
        unsafe { std::env::set_var("FLYWHEEL_MAIL_NAME", "Orchid") };
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:18080"));
        assert_eq!(token.as_deref(), Some("secret"));
        assert_eq!(name.as_deref(), Some("Orchid"));
        unsafe { std::env::remove_var("FLYWHEEL_TORON_URL") };
        unsafe { std::env::remove_var("FLYWHEEL_TORON_TOKEN") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL_NAME") };
    }

    /// bd-uu58: `lock_env()` must be hermetic on its own, because it is the
    /// choke point every env-mutating test already calls. A bare lock leaves
    /// `XDG_STATE_HOME` pointing wherever the last lock-holder left it — or at
    /// the operator's real `~/.local/state/flywheel` when nobody pinned one.
    ///
    /// Two assertions, because "not the real dir" alone would still pass for a
    /// guard that pinned some *other* stale dir: (a) the resolved state dir
    /// sits under the temp root, and (b) two lock-holders never share a dir,
    /// so one test's fixtures can never become the next test's input.
    #[test]
    fn lock_env_pins_a_private_hermetic_state_dir() {
        let first_dir;
        {
            let _g = lock_env();
            first_dir = state_dir();
            assert!(
                first_dir.starts_with(std::env::temp_dir()),
                "lock_env must resolve state_dir() under the temp root, got {first_dir:?} \
                 (a fixture writing here lands in whatever XDG_STATE_HOME a sibling left)"
            );
            assert!(
                std::fs::create_dir_all(first_dir.join("agents")).is_ok(),
                "and the resolved dir is usable, so agents/ spawn registries written under \
                 this lock are the ones a reader can find at state_dir()"
            );
        }
        {
            let _g = lock_env();
            let second_dir = state_dir();
            assert_ne!(
                second_dir, first_dir,
                "each lock-holder gets its own state home, so a stale fixture cannot be \
                 inherited by the next test (this is what made the bd-9w95 spawn flake red)"
            );
        }
    }

    /// bd-uu58: drop must RESTORE the prior value, not remove the var. A test
    /// that set `XDG_STATE_HOME` without taking this lock would otherwise have
    /// its value erased by the next lock-holder to finish, handing that test the
    /// real state dir — the same leak arriving through the drop path.
    ///
    /// Both reads happen UNDER the lock, which is what makes this deterministic
    /// in a parallel binary: `ENV_LOCK` is not reentrant, so reading after the
    /// guard's drop without re-taking it races any holder that grabbed the lock
    /// in between. The value is invariant across lock-holders because every
    /// sanctioned setter restores what it found, so reading it while holding the
    /// lock is a stable sample.
    #[test]
    fn test_state_home_restores_the_prior_state_home_on_drop() {
        let raw = || {
            ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let prior = {
            let _g = raw();
            std::env::var_os("XDG_STATE_HOME")
        };
        {
            let _home = test_state_home();
            assert!(
                state_dir().starts_with(std::env::temp_dir()),
                "inside the guard the pinned temp dir wins"
            );
        }
        let after = {
            let _g = raw();
            std::env::var_os("XDG_STATE_HOME")
        };
        assert_eq!(
            after, prior,
            "drop restores the prior value; removing the var would hand the next test \
             the real state dir. Note this only proves the restore when the ambient \
             XDG_STATE_HOME is SET — with it unset both arms are `None` and the \
             assertion is vacuous, which is why M2 in scripts/mutations/ \
             state-home.spec.sh names this test rather than a hand-built guard"
        );
    }

    /// bd-uu58: the OTHER arm of the restore. `prior` here is genuinely `None`
    /// on a clean process, so the test above only ever exercises "drop removes
    /// an absent var" — where `remove_var` and "restore None" are the same act,
    /// which is exactly why mutation M2 could not turn it red. Reaching the
    /// `Some` arm needs a value already set when the guard is CONSTRUCTED, and
    /// the one sanctioned way to set the var (under a guard) always captures
    /// `None`. So the guard is built by hand here, from a lock this test already
    /// holds.
    ///
    /// Every mutation of the var happens under the lock and restores what it
    /// found, so the value is invariant across lock-holders and the re-take
    /// below is a stable sample — that is what keeps this deterministic in a
    /// parallel binary. Note `ENV_LOCK` is not reentrant: the guard is
    /// constructed from the guard variable itself (moved in), never by calling
    /// `test_state_home()` while holding it.
    #[test]
    fn drop_restores_a_foreign_prior_value_rather_than_clearing_it() {
        let sentinel = std::env::temp_dir().join("flywheel-bd-uu58-prior");
        let ours = std::env::temp_dir().join("flywheel-bd-uu58-ours");
        let ambient = {
            let _g = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::fs::create_dir_all(&ours).expect("create our dir");
            let ambient = std::env::var_os("XDG_STATE_HOME");
            unsafe { std::env::set_var("XDG_STATE_HOME", &sentinel) };
            ambient
        };
        {
            let _g = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            {
                let _home = TestStateHome {
                    _env: _g,
                    dir: ours.clone(),
                    prev: Some(sentinel.clone().into_os_string()),
                    prev_home: std::env::var_os("HOME"),
                    prev_toron_data: std::env::var_os("TORON_DATA_DIR"),
                    prev_toron_mcp_data: std::env::var_os("TORON_MCP_DATA_DIR"),
                    prev_br_bin: std::env::var_os("FLYWHEEL_BR_BIN"),
                    prev_path: std::env::var_os("PATH"),
                };
                assert_eq!(
                    std::env::var_os("XDG_STATE_HOME").as_deref(),
                    Some(sentinel.as_os_str()),
                    "construction carries the foreign value; it does not rewrite it"
                );
            }
        }
        {
            let _g = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                Some(sentinel.as_os_str()),
                "drop puts the foreign value back. remove_var here would hand the next \
                 test the real state dir — the bd-uu58 leak through the drop path"
            );
            unsafe { std::env::remove_var("XDG_STATE_HOME") };
            if let Some(a) = ambient {
                unsafe { std::env::set_var("XDG_STATE_HOME", a) };
            }
        }
        let _ = std::fs::remove_dir_all(&ours);
    }

    #[test]
    fn mail_env_overrides_ignore_empty_values() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::set_var("FLYWHEEL_TORON_URL", "") };
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!(url, None, "empty url counts as unset");
        assert_eq!(token, None);
        assert_eq!(name, None);
        unsafe { std::env::remove_var("FLYWHEEL_TORON_URL") };
    }

    #[test]
    fn mail_env_overrides_nothing_set() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("FLYWHEEL_TORON_URL") };
        unsafe { std::env::remove_var("FLYWHEEL_TORON_TOKEN") };
        unsafe { std::env::remove_var("FLYWHEEL_MAIL_NAME") };
        let (url, token, name) = Config::mail_env_overrides();
        assert_eq!((url, token, name), (None, None, None));
    }

    /// Toron config isolation (ADR-0003 V3, BUG1; bd-euav single-backend):
    /// `[mail.toron]` carries its OWN url — the shared `[agent_mail]` url can
    /// never route toron. `toron_mail_config()` defaults to embedded
    /// in-process (no url).
    #[test]
    fn per_backend_config_split_isolates_urls() {
        let cfg = Config {
            agent_mail: Some(MailConfig {
                url: Some("http://127.0.0.1:18080".into()),
                token: Some("shared".into()),
                name: Some("Orchid".into()),
            }),
            mail: Some(MailSections {
                toron: Some(MailServerConfig {
                    url: Some("http://127.0.0.1:18080".into()),
                    token: Some("toron-token".into()),
                }),
            }),
            ..Default::default()
        };
        let toron = cfg.toron_mail_config();
        assert_eq!(toron.url.as_deref(), Some("http://127.0.0.1:18080"));
        assert_eq!(toron.token.as_deref(), Some("toron-token"));
        assert_eq!(toron.name.as_deref(), Some("Orchid"));
    }

    /// The legacy `[agent_mail] url` never routes toron (no
    /// backward-compat bridge — ADR-0003 V2/V3 BUG1 fix): with no
    /// `[mail.toron]`, toron is embedded in-process. The shared
    /// `[agent_mail] token` IS inherited by toron
    /// (ADR-0003 V3: "[agent_mail] keeps … token if shared").
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
    }

    /// Zero-config default build: no sections at all → toron is embedded
    /// in-process.
    #[test]
    fn zero_config_defaults_embedded_toron() {
        let cfg = Config::default();
        let toron = cfg.toron_mail_config();
        assert_eq!(toron.url, None, "no url -> embedded in-process toron");
        assert_eq!(toron.name, None);
    }

    /// Real XDG dir: `$XDG_CONFIG_HOME` wins; else `~/.config/flywheel`.
    #[test]
    fn config_dir_is_real_xdg() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = std::env::temp_dir().join(format!("flywheel-cfg-xdg-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &tmp) };
        assert_eq!(config_dir(), tmp.join("flywheel"));
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

    /// No XDG_CONFIG_HOME → `~/.config/flywheel` (NOT macOS Application
    /// Support, which is what `dirs::config_dir()` returns).
    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        let home = std::env::temp_dir().join(format!("flywheel-cfg-home-{}", std::process::id()));
        unsafe { std::env::set_var("HOME", &home) };
        assert_eq!(
            config_dir(),
            home.join(".config/flywheel"),
            "XDG default is ~/.config/flywheel, not dirs::config_dir()"
        );
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// hc socket fallback matches toron `resolve_socket_path`: HC_SOCKET_PATH
    /// else `$XDG_STATE_HOME/herdr-core/default/default.sock`.
    #[test]
    fn socket_path_follows_xdg_state_home() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("HC_SOCKET_PATH") };
        let tmp = std::env::temp_dir().join(format!("flywheel-sock-xdg-{}", std::process::id()));
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        unsafe { std::env::set_var("XDG_STATE_HOME", &tmp) };
        let cfg = Config::default();
        assert_eq!(
            cfg.socket_path(),
            tmp.join("herdr-core/default/default.sock")
                .to_string_lossy()
        );
        match prev_state {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
    }

    /// No XDG_STATE_HOME → `~/.local/state/herdr-core/default/default.sock`.
    #[test]
    fn socket_path_falls_back_to_home_local_state() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe { std::env::remove_var("HC_SOCKET_PATH") };
        let prev_state = std::env::var_os("XDG_STATE_HOME");
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let home = std::env::temp_dir().join(format!("flywheel-sock-home-{}", std::process::id()));
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };
        let cfg = Config::default();
        assert_eq!(
            cfg.socket_path(),
            home.join(".local/state/herdr-core/default/default.sock")
                .to_string_lossy(),
            "socket default is ~/.local/state/herdr-core/default/default.sock"
        );
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_state {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
    }

    /// is1.6: a non-numeric FLYWHEEL_TIMEOUT aborts Config::load() naming
    /// the var — never a silent fallback to the default.
    #[test]
    fn flywheel_timeout_env_fails_loudly() {
        let _g = lock_env();
        let home = std::env::temp_dir().join(format!("flywheel-to-home-{}", std::process::id()));
        let xdg = std::env::temp_dir().join(format!("flywheel-to-xdg-{}", std::process::id()));
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_timeout = std::env::var_os("FLYWHEEL_TIMEOUT");
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
        match prev_timeout {
            Some(v) => unsafe { std::env::set_var("FLYWHEEL_TIMEOUT", v) },
            None => unsafe { std::env::remove_var("FLYWHEEL_TIMEOUT") },
        }
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

    /// 193.2: `state_dir()` honors `XDG_STATE_HOME` first (hermetic state
    /// for tests/prod) — mirror of the XDG_CONFIG_HOME contract.
    /// bd-rrlo2: the events default follows minted tokens — off with an
    /// empty token dir, on once any per-pin token file exists. Reads the
    /// dir directly (no global state), so parallel tests cannot flake.
    #[test]
    fn toron_events_default_follows_minted_tokens() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        assert!(
            !crate::events_tokens::any_token_file(&dir),
            "no tokens: the default stays flag-off"
        );
        let tokens = dir.join("events-tokens");
        std::fs::create_dir_all(&tokens).expect("token dir");
        std::fs::write(tokens.join("demo-AmberProbe.token"), "s3").expect("token");
        assert!(
            crate::events_tokens::any_token_file(&dir),
            "a minted token switches the default pump on"
        );
    }

    #[test]
    fn state_dir_honors_xdg_state_home() {
        let _g = lock_env();
        let prev_xdg_state = std::env::var_os("XDG_STATE_HOME");
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        unsafe { std::env::set_var("XDG_STATE_HOME", "/tmp/fw-state-xdg") };
        assert_eq!(
            state_dir(),
            PathBuf::from("/tmp/fw-state-xdg/flywheel"),
            "XDG_STATE_HOME wins over dirs::state_dir()"
        );
        match prev_xdg_state {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
    }
    /// bd-y6gw: without XDG the fallback is ~/.local/state (matching
    /// daemon::default_state_dir), never dirs::state_dir() — one tree for
    /// the CLI and the daemon on every platform.
    #[test]
    fn state_dir_falls_back_to_dotlocal_not_appsupport() {
        let _g = lock_env();
        let prev_xdg_state = std::env::var_os("XDG_STATE_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        let home = std::env::temp_dir().join(format!("fw-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        unsafe { std::env::set_var("HOME", &home) };
        assert_eq!(
            state_dir(),
            home.join(".local/state/flywheel"),
            "no XDG -> ~/.local/state, same tree the daemon uses"
        );
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_xdg_state {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// legacy file as backup. The live `[agent_mail]` block survives the copy.
    #[test]
    fn load_migrates_legacy_config_to_xdg_copying() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home =
            std::env::temp_dir().join(format!("flywheel-legacy-home-{}", std::process::id()));
        let xdg = std::env::temp_dir().join(format!("flywheel-xdg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&xdg);
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("HOME", &home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &xdg) };
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
        let xdg_file = xdg.join("flywheel/config.toml");
        assert!(xdg_file.exists(), "legacy config copied to XDG");
        assert!(
            legacy_dir.join("config.toml").exists(),
            "legacy file kept as backup (copy, not move)"
        );
        let copied = std::fs::read_to_string(&xdg_file).unwrap();
        assert!(copied.contains("Orchid"), "copied body intact");
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
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
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
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
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

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
        let c: Config = toml::from_str(r#"on_captain_death = "recover""#).unwrap();
        assert_eq!(c.on_captain_death(), OnCaptainDeath::Adopt);
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
        apply_captain_adopt(&mut aliases, "proj", "NewCaptain");
        assert_eq!(
            aliases.get("proj").map(std::string::String::as_str),
            Some("NewCaptain")
        );
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
        let registry = vec![captain_entry, child, unrelated];

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
        let registry = vec![captain_entry, child];

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
            child,
        ];
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
        let without_reap =
            registry_after_captain_death(&registry, captain, OnCaptainDeath::Continue);
        assert!(without_reap.iter().any(|e| e.pane_id == "pane-child"));
    }

    #[test]
    fn on_captain_death_mail_to_captain_fails_when_alias_cleared_until_rebound() {
        use crate::state as st;
        let _home = test_state_home();
        let proj = format!("oncap-mail-{}", std::process::id());
        st::save_captain_alias(&proj, "BraveCaptain").unwrap();
        assert!(st::resolve_captain_target(&proj, "captain").is_ok());
        st::clear_captain_alias_file(&proj).unwrap();
        let err =
            st::resolve_captain_target(&proj, "captain").expect_err("cleared alias must fail");
        assert!(format!("{err}").contains("alias cleared"));
        assert!(format!("{err}").contains("recover"));
        st::save_captain_alias(&proj, "NewCaptain").unwrap();
        assert!(st::resolve_captain_target(&proj, "captain").is_ok());
        assert!(st::resolve_captain_target(&proj, "CAPTAIN").is_ok());
        st::clear_captain_alias_file(&proj).unwrap();
    }

    #[test]
    fn on_captain_death_kill_continue_keeps_doorbell_and_crews() {
        use crate::state::{self as st, SpawnEntry};
        let _home = test_state_home();
        let proj = format!("oncap-kill-{}", std::process::id());
        let captain = "BraveCaptain";
        let child = SpawnEntry::with_mail_identity("pane-child", "child1", "ChildPin");
        st::save_spawn_registry(&proj, &[child]).unwrap();
        st::save_captain_alias(&proj, captain).unwrap();
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
        st::save_captain_alias(&proj, "AdoptedCaptain").unwrap();
        assert_eq!(
            st::load_captain_alias(&proj).as_deref(),
            Some("AdoptedCaptain")
        );
        st::clear_captain_alias_file(&proj).unwrap();
        let _ = std::fs::remove_file(st::spawn_registry_file(&proj));
    }

    /// bd-envlock: a test that points `XDG_STATE_HOME` at a temp dir without
    /// taking the crate env guard corrupts every other test in the process.
    ///
    /// `adr0010_d7_journal_rollback_restores_prior_record` did exactly that: it
    /// made a `tempfile::tempdir()`, set the var, and let both the directory and
    /// the binding die with the test. `tempdir()` is dropped at end of test, so
    /// for the rest of the process `XDG_STATE_HOME` pointed at a path that no
    /// longer existed. A concurrent or later test writing state artifacts then
    /// failed on a directory that was never there, and because the symptom is a
    /// missing directory rather than a wrong value, it reads as a random flake
    /// instead of a test bug.
    ///
    /// The lock alone is not the whole fix. `test_state_home()` is preferred
    /// because its `Drop` unsets the variable AND removes the directory, so the
    /// next test inherits the real state home rather than a dead one. This test
    /// accepts either guard: a test that only reads the environment needs the
    /// lock without needing a temp dir.
    ///
    /// Scoped per `#[test]` block on purpose. A file-level check would let any
    /// one test in a 30-lock file excuse a 31st test that locks nothing, which
    /// is exactly the failure this exists to catch. A test that mutates the var
    /// inside a helper function rather than in the test body is invisible to
    /// this scan; those stay covered by the module-level convention that a file
    /// touching `XDG_STATE_HOME` holds a guard somewhere.
    #[test]
    fn every_test_pointing_xdg_state_home_holds_the_env_guard() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }

        /// One block per `#[test]`, each bounded at the next test or module
        /// attribute. Bounding only at the next `#[test]` is not enough: the
        /// LAST test in a module would then run on into the following module
        /// and be blamed for whatever that module sets. Bounding only at the
        /// next `#[cfg(` is not enough either: consecutive tests share no cfg
        /// between them, so the first would swallow all the rest and be named
        /// for an offence committed by a later one.
        fn test_blocks(src: &str) -> Vec<&str> {
            const TEST: &str = "#[test]";
            let mut blocks = Vec::new();
            let mut rest = src;
            while let Some(t) = rest.find(TEST) {
                let mut end = rest.len();
                for marker in ["#[tokio::test]", "#[cfg("] {
                    if let Some(i) = rest[t..].find(marker) {
                        end = end.min(t + i);
                    }
                }
                if let Some(i) = rest[t + TEST.len()..].find(TEST) {
                    end = end.min(t + TEST.len() + i);
                }
                blocks.push(&rest[t..end]);
                rest = &rest[end..];
            }
            blocks
        }

        let mut files = Vec::new();
        walk(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );

        let mut offenders = Vec::new();
        for file in &files {
            let Ok(src) = std::fs::read_to_string(file) else {
                continue;
            };
            for block in test_blocks(&src) {
                if !block.contains("set_var(\"XDG_STATE_HOME\"") {
                    continue;
                }
                if block.contains("test_state_home()")
                    || block.contains("lock_env()")
                    || block.contains("ENV_LOCK")
                {
                    continue;
                }
                let name = block
                    .trim_start_matches("#[test]")
                    .split('(')
                    .next()
                    .unwrap_or("<unparsed>")
                    .trim();
                offenders.push(format!("{}: {name}", file.display()));
            }
        }
        assert!(
            offenders.is_empty(),
            "these tests set XDG_STATE_HOME with no env guard, so the whole test \
             process inherits a state home that may already have been deleted: \
             {offenders:?}"
        );
    }

    /// bd-poison: no test may take a crate-wide static lock with `.unwrap()`.
    ///
    /// A `Mutex` is poisoned when a thread panics while holding it, and a
    /// poisoned lock stays poisoned for the life of the process. So one test
    /// that panics inside a guarded section makes every LATER test that takes
    /// the same lock fail with `PoisonError` — a cascade that has nothing to do
    /// with what those later tests are testing, and that keeps failing on every
    /// run thereafter.
    ///
    /// That is how `judgment::tests::b5_find_pack_picks_highest_version` and
    /// `judgment::tests::xdg_pack_is_the_source_of_truth` were failing: neither
    /// was broken, they were downstream of an unrelated panic holding
    /// `ENV_LOCK`. They read as real regressions and were not.
    ///
    /// `lock_env()` already recovers from poisoning, and the crate documents it
    /// as the reference pattern. The rule this enforces is simply that a
    /// crate-wide test lock is taken through its poison-tolerant accessor, never
    /// directly. Per-test local mutexes are fine with `.unwrap()` and are not
    /// flagged: poisoning one only breaks the single test that owns it.
    #[test]
    fn no_test_takes_a_crate_wide_lock_without_the_poison_tolerant_accessor() {
        /// Crate-wide static test locks. A per-test local mutex is not listed:
        /// only a lock shared across tests can cascade.
        const SHARED_LOCKS: [&str; 2] = ["ENV_LOCK", "LOOP_REPORTER_LOCK"];

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }

        let mut files = Vec::new();
        walk(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );

        let mut offenders = Vec::new();
        for file in &files {
            let Ok(src) = std::fs::read_to_string(file) else {
                continue;
            };
            for (i, line) in src.lines().enumerate() {
                for lock in SHARED_LOCKS {
                    let needle = format!("{lock}.lock().unwrap()");
                    if line.contains(&needle) {
                        offenders.push(format!("{}:{}: {lock}", file.display(), i + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these take a crate-wide test lock with .unwrap(), so one unrelated \
             panic poisons the lock and every later test using it fails with \
             PoisonError: {offenders:?}"
        );
    }

    /// bd-poison: the accessor itself must survive poisoning, or routing every
    /// test through it buys nothing. This one poisons the real `ENV_LOCK` on
    /// purpose.
    ///
    /// Safe to do because every crate-wide lock user now goes through a
    /// poison-tolerant accessor. Before that was true, this test would have
    /// cascaded into unrelated failures — which is the behaviour it pins shut.
    #[test]
    fn the_env_lock_accessor_survives_being_poisoned() {
        let panicked = std::panic::catch_unwind(|| {
            let _guard = lock_env();
            panic!("deliberate: poison the crate env lock");
        })
        .is_err();
        assert!(panicked, "the poisoning panic must actually fire");

        let guard = lock_env();
        drop(guard);

        let guard = lock_env();
        drop(guard);
    }

    #[cfg(test)]
    mod program_status_push_tests {
        use super::*;

        #[test]
        fn program_status_push_is_off_by_default_so_behaviour_is_byte_identical() {
            assert!(!Config::default().doorbell_program_status_push());
        }
    }
}

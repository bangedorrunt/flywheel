// governed-by: ADR-0015
// governed-by: ADR-0016
//! Injected host authority (P4). Resolve once at `main()`, thread down.
//! Call sites take `&HostAuthority`; env-mutating tests banned for new code.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct HostAuthority {
    pub home: String,
    pub cwd: std::path::PathBuf,
    pub exe: std::path::PathBuf,
    /// Captured once so effect interpreters do not read ambient credentials.
    pub typesafe_api_key: Option<String>,
}

/// The ONE identity ladder (bd-05ar).
///
/// `explicit` → `TORON_AGENT` → `FLYWHEEL_MAIL_AS` → `AGENT_NAME` →
/// `config_name`.
///
/// The three env rungs are toron's `guard_agent` attribution order EXACTLY,
/// and that parity is the whole point: the lease holder (toron's strict
/// guard) and the commit author (the same guard) must be the SAME pin, and
/// the doorbell actor must be that pin again. Three sites re-implemented
/// this ladder with three different orders — `AGENT_NAME` first, then
/// `FLYWHEEL_MAIL_AS` first, then `TORON_AGENT` missing entirely — so a
/// lease could be held under one pin while the commit was attributed to
/// another. One ladder, one order, one implementation.
///
/// `config_name` is the `[agent_mail].name` rung: it exists only where a
/// [`crate::config::Config`] is in scope, and it is always LAST.
///
/// The ambient [`HostAuthority`] deliberately holds NO identity: a cached
/// answer is a second answer, and a cached answer is the one that drifts.
pub fn select_agent_identity(
    explicit: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    config_name: Option<&str>,
) -> Option<String> {
    if let Some(v) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(v.to_string());
    }
    for key in ["TORON_AGENT", "FLYWHEEL_MAIL_AS", "AGENT_NAME"] {
        if let Some(v) = env(key)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(v);
        }
    }
    config_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Wall-clock authority. Reducers never own or read this trait; effects use it
/// to stamp observations before sending time back as a message.
#[allow(dead_code)]
pub trait Clock: Send + Sync {
    fn now_unix(&self) -> u64;
    /// Nanosecond resolution, for ids that must differ WITHIN a second.
    /// Second resolution would collide two invocations in the same second
    /// and silently re-run a settled promise (bd-bvwf.4 resume contract).
    fn now_nanos(&self) -> u128 {
        u128::from(self.now_unix()) * 1_000_000_000
    }
}

/// Monotonic authority for cooldowns and deadlines. It is intentionally
/// separate from wall time, which can jump during clock correction.
#[allow(dead_code)]
pub trait MonotonicClock: Send + Sync {
    fn now_millis(&self) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
#[allow(dead_code)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs())
    }

    fn now_nanos(&self) -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SystemMonotonicClock {
    origin: std::time::Instant,
}

impl Default for SystemMonotonicClock {
    fn default() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl MonotonicClock for SystemMonotonicClock {
    fn now_millis(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct FixedClock {
    pub now: u64,
}

impl Clock for FixedClock {
    fn now_unix(&self) -> u64 {
        self.now
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct FixedMonotonicClock {
    pub now: u64,
}

impl MonotonicClock for FixedMonotonicClock {
    fn now_millis(&self) -> u64 {
        self.now
    }
}

/// Resolved process dependencies passed to effect interpreters. New effectful
/// paths should accept this value rather than reading ambient process state.
#[derive(Clone)]
#[allow(dead_code)]
pub struct RuntimeAuthority {
    pub host: HostAuthority,
    pub clock: Arc<dyn Clock>,
    pub monotonic: Arc<dyn MonotonicClock>,
}

#[allow(dead_code)]
impl RuntimeAuthority {
    pub fn resolve() -> Self {
        Self {
            host: HostAuthority::resolve(),
            clock: Arc::new(SystemClock),
            monotonic: Arc::new(SystemMonotonicClock::default()),
        }
    }

    #[cfg(test)]
    pub fn for_test(home: &str, now: u64) -> Self {
        Self {
            host: HostAuthority::for_test(home),
            clock: Arc::new(FixedClock { now }),
            monotonic: Arc::new(FixedMonotonicClock { now }),
        }
    }

    pub fn now_unix(&self) -> u64 {
        self.clock.now_unix()
    }

    /// Wall time as a chrono `DateTime`, read from the INJECTED clock.
    ///
    /// bd-lf1u: a durable leaf that stamps DATA with a timestamp must not call
    /// `chrono::Utc::now()` — on replay the stamped value would differ from
    /// the original run's. This is the one conversion, so the clock read stays
    /// in the authority layer and the leaf keeps a single injected source.
    ///
    /// Second resolution is deliberate: this stamps evidence rows, not
    /// ordering keys. Anything needing sub-second distinctness uses
    /// `clock.now_nanos()` directly (see the `Clock` doc comment).
    pub fn now_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(self.clock.now_unix() as i64, 0)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH)
    }
}

impl HostAuthority {
    /// Resolve process authority once per process. Cloning the snapshot is
    /// cheap; re-reading environment and process paths at each effect site is
    /// precisely the ambient-authority drift this seam prevents.
    pub fn resolve() -> Self {
        static RESOLVED: std::sync::OnceLock<HostAuthority> = std::sync::OnceLock::new();
        RESOLVED.get_or_init(Self::resolve_uncached).clone()
    }

    fn resolve_uncached() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("flywheel"));
        let typesafe_api_key = std::env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty());
        Self {
            home,
            cwd,
            exe,
            typesafe_api_key,
        }
    }

    #[cfg(test)]
    pub fn for_test(home: &str) -> Self {
        Self {
            home: home.into(),
            cwd: std::path::PathBuf::from("/tmp/fw-test"),
            exe: std::path::PathBuf::from("flywheel"),
            typesafe_api_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_constructs_without_env_mutation() {
        let a = HostAuthority::for_test("/tmp/fw-home");
        assert_eq!(a.home, "/tmp/fw-home");
        assert!(a.typesafe_api_key.is_none());
    }

    #[test]
    fn resolve_never_panics_without_env() {
        let a = HostAuthority::resolve();
        assert_ne!(a.home, "");
    }

    #[test]
    fn runtime_authority_uses_injected_clock() {
        let authority = RuntimeAuthority::for_test("/tmp/fw-home", 42);
        assert_eq!(authority.now_unix(), 42);
        assert_eq!(authority.host.cwd, std::path::Path::new("/tmp/fw-test"));
        assert_eq!(authority.monotonic.now_millis(), 42);
    }

    /// bd-05ar: every site that answers "who am I" MUST answer with the ONE
    /// ladder, and the ladder MUST run in toron's guard order
    /// (`TORON_AGENT` > `FLYWHEEL_MAIL_AS` > `AGENT_NAME`).
    ///
    /// Before the fix each site had its own order: the ambient authority read
    /// `AGENT_NAME` first, the comms sender read `FLYWHEEL_MAIL_AS` first and
    /// never read `TORON_AGENT` at all, and the doorbell inherited the ambient
    /// authority's order. So with `AGENT_NAME=A` and `TORON_AGENT=B` the
    /// lease holder resolved one pin and the commit author another — the
    /// failure the audit measured.
    ///
    /// The ambient authority no longer caches an identity at all: a cached
    /// answer is a second answer, and it is the one that drifts.
    #[test]
    fn identity_ladder_is_shared_by_every_resolving_site() {
        let env = |k: &str| match k {
            "TORON_AGENT" => Some("ToronPin".to_string()),
            "FLYWHEEL_MAIL_AS" => Some("MailAsPin".to_string()),
            "AGENT_NAME" => Some("AgentNamePin".to_string()),
            _ => None,
        };
        assert_eq!(
            select_agent_identity(None, &env, None).as_deref(),
            Some("ToronPin"),
            "the ladder must resolve TORON_AGENT first"
        );
        assert_eq!(
            crate::comms_send::sender_token(None, None, &env).unwrap(),
            "ToronPin",
            "the comms sender must resolve TORON_AGENT first"
        );
        assert_eq!(
            crate::cmd::doorbell::resolve_as_pin(None, None, &env).as_deref(),
            Some("ToronPin"),
            "the doorbell actor must resolve TORON_AGENT first"
        );
    }

    /// bd-05ar: the pure selection half of the ladder. Explicit wins, the env
    /// rungs run in the guard's order, empty values never win, and the
    /// configured name is the LAST rung.
    #[test]
    fn reservation_identity_selection_matches_guard_order() {
        let env = |k: &str| match k {
            "TORON_AGENT" => None,
            "FLYWHEEL_MAIL_AS" => Some("MailAs".into()),
            "AGENT_NAME" => Some("AgentName".into()),
            _ => None,
        };
        // explicit beats everything
        assert_eq!(
            select_agent_identity(Some("Explicit"), &env, None).as_deref(),
            Some("Explicit")
        );
        // FLYWHEEL_MAIL_AS before AGENT_NAME (TORON_AGENT unset here)
        assert_eq!(
            select_agent_identity(None, &env, None).as_deref(),
            Some("MailAs")
        );
        // TORON_AGENT jumps the queue when set (guard parity)
        let env_t = |k: &str| match k {
            "TORON_AGENT" => Some("Toron".into()),
            "FLYWHEEL_MAIL_AS" => Some("MailAs".into()),
            "AGENT_NAME" => Some("AgentName".into()),
            _ => None,
        };
        assert_eq!(
            select_agent_identity(None, &env_t, None).as_deref(),
            Some("Toron")
        );
        // empty-string vars are skipped, not selected
        let env_empty = |k: &str| match k {
            "TORON_AGENT" => Some("  ".into()),
            "FLYWHEEL_MAIL_AS" => Some("MailAs".into()),
            _ => None,
        };
        assert_eq!(
            select_agent_identity(None, &env_empty, None).as_deref(),
            Some("MailAs")
        );
        // the configured name is the LAST rung
        assert_eq!(
            select_agent_identity(None, &|_| None, Some("ConfigPin")).as_deref(),
            Some("ConfigPin")
        );
        // nothing anywhere -> no identity
        assert_eq!(select_agent_identity(None, &|_| None, None), None);
    }

    /// bd-lf1u: `now_datetime` must DERIVE from the injected clock, not from
    /// the ambient one.
    ///
    /// The leaf-body guard can only prove the leaf stopped calling
    /// `chrono::Utc::now()` directly. That is a source-shape fact, and it was
    /// proven insufficient here: rewriting `now_datetime` to call
    /// `SystemTime::now()` behind the injection point left every bd-lf1u test
    /// green, because the ambient read had simply moved one call away. So the
    /// property under test is behavioural — a fixed clock yields THAT instant,
    /// and two different fixed clocks yield two different values — which is
    /// what actually distinguishes injected time from wall time.
    #[test]
    fn bd_lf1u_now_datetime_derives_from_the_injected_clock() {
        let early = RuntimeAuthority::for_test("/tmp/fw-home", 1_000_000_000);
        let late = RuntimeAuthority::for_test("/tmp/fw-home", 1_700_000_000);

        let a = early.now_datetime();
        let b = late.now_datetime();

        assert_eq!(
            a.timestamp(),
            1_000_000_000,
            "now_datetime must return the injected instant, not the wall clock"
        );
        assert_eq!(b.timestamp(), 1_700_000_000);
        assert_ne!(
            a, b,
            "two injected clocks must produce two different timestamps; equal values \
             mean the injected clock is being ignored and ambient time is winning"
        );

        // Agreement between the two readings off the same authority: a
        // DateTime that disagreed with the clock it came from would mean the
        // conversion silently substituted a different source.
        assert_eq!(
            a.timestamp() as u64,
            early.now_unix(),
            "now_datetime and now_unix must read the SAME clock"
        );

        // And it must be the real calendar, not a synthetic counter: this
        // instant is 2001-09-09T01:46:40Z.
        assert_eq!(
            a.to_rfc3339(),
            "2001-09-09T01:46:40+00:00",
            "the conversion must be a real calendar conversion of the injected instant"
        );
    }
}

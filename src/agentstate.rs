//! Agent state file contract: `<project>/.flywheel/agent-state/<name>.json`.
//!
//! The durable loop's context-exhaustion respawn depends on a signal the AGENT
//! decides (~80% session usage). A file is deterministic (read at poll time via
//! a leaf — replay-safe), survives herdr's alternate-screen caveat, and is
//! advisory: the agent never blocks its own work on the write.
//!
//! Contract (single-purpose for now — context signal only):
//! ```json
//! { "context_pct": 82, "ts": "2026-08-03T03:00:00Z" }
//! ```
//! `context_pct` is 0..=100; `ts` is RFC3339 (same shape as `state::now_iso`).
//!
//! Freshness matters: a stale file from a previous session must NEVER trigger
//! a respawn — a healthy agent would be killed for no reason. Default max_age
//! 15 minutes: longer than any plausible human gap, shorter than a lunch break.
//!
//! Most of this API is a published contract consumed by dependent beads
//! (`bd-prompt-state-file` writer, `bd-read-state-fallback-zwa` reader).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One agent state file, as read from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStateFile {
    /// 0..=100; written when the session context reaches ~80%.
    pub context_pct: u8,
    /// RFC3339 write time (seconds resolution, `Z` or `±HH:MM` accepted).
    pub ts: String,
}

/// Default freshness window for agent state files.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_mins(15);

/// Why an agent name was rejected for path building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    Traversal,
    BadChar,
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "agent name is empty"),
            Self::Traversal => write!(f, "agent name contains a path traversal component"),
            Self::BadChar => write!(f, "agent name contains an unsafe character"),
        }
    }
}

/// The project-relative state directory: `<project>/.flywheel/agent-state/`.
pub fn project_state_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".flywheel").join("agent-state")
}

/// Path of one agent's state file: `<project>/.flywheel/agent-state/<name>.json`.
///
/// Rejects empty names, `.`/`..` traversal components, path separators, NUL,
/// and control characters — a name must always be a single path component.
pub fn state_file_path(project_dir: &Path, name: &str) -> Result<PathBuf, NameError> {
    validate_name(name)?;
    Ok(project_state_dir(project_dir).join(format!("{name}.json")))
}

/// Validate an agent name for use as a single file path component.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name == "." || name == ".." {
        return Err(NameError::Traversal);
    }
    for c in name.chars() {
        if c == '/' || c == '\\' {
            return Err(NameError::Traversal);
        }
        if c.is_control() || c == '\u{0}' {
            return Err(NameError::BadChar);
        }
    }
    Ok(())
}

/// Parse one agent's state file, if present and well-formed.
pub fn read_one_state_file(project_dir: &Path, name: &str) -> Option<AgentStateFile> {
    let p = state_file_path(project_dir, name).ok()?;
    let content = std::fs::read_to_string(p).ok()?;
    parse_state_file(&content)
}

/// Read an agent's saturation state trying BOTH identity files: the herdr
/// pane name AND (when different) the spawn-registry mail pin. The standing-
/// orders contract substitutes `{AGENT_NAME}` with the PIN (the identity the
/// crew is addressed by), so a pin-identity crew writes `<pin>.json` while
/// the loop keys panes by herdr name — checking only the herdr name made the
/// file channel permanently blind for exactly those agents
/// (bd-context-limit-honor-luqg). The FRESHER file wins when both exist.
pub fn read_state_for_agent(
    project_dir: &Path,
    herdr_name: &str,
    mail_pin: Option<&str>,
) -> Option<AgentStateFile> {
    let mut best: Option<(u64, AgentStateFile)> = None;
    for name in std::iter::once(herdr_name).chain(mail_pin.into_iter().filter(|p| *p != herdr_name))
    {
        let Some(f) = read_one_state_file(project_dir, name) else {
            continue;
        };
        let ts = parse_ts(&f.ts).unwrap_or(0);
        if best.as_ref().is_none_or(|(bts, _)| ts >= *bts) {
            best = Some((ts, f));
        }
    }
    best.map(|(_, f)| f)
}

/// Parse an agent state file. Malformed JSON, out-of-range `context_pct`, or an
/// unparseable `ts` all yield `None` — never an error or panic.
pub fn parse_state_file(content: &str) -> Option<AgentStateFile> {
    let f: AgentStateFile = serde_json::from_str(content).ok()?;
    if f.context_pct > 100 {
        return None;
    }
    parse_ts(&f.ts)?;
    Some(f)
}

/// Parse an RFC3339 timestamp to unix seconds.
///
/// Accepts the shapes `state::now_iso` produces (`2026-08-03T03:00:00Z`) plus
/// fractional seconds and `±HH:MM` offsets (chrono's strict RFC3339 parser;
/// is1.2 — one calendar implementation in the crate). Any other format →
/// `None` (never panics). Pre-1970 → `None` (the fresh/stale heuristic has no
/// use for negative epochs). Leap-second `:60` timestamps are rejected by
/// chrono (RFC3339-strict; the old hand-rolled parser tolerated them).
pub fn parse_ts(ts: &str) -> Option<u64> {
    // chrono's RFC3339 parser tolerates a SPACE date-time separator; the
    // contract (`state::now_iso` shape) requires the literal 'T' — pin it.
    if ts.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let unix = dt.timestamp();
    if unix < 0 { None } else { Some(unix as u64) }
}

/// Is the state file fresh relative to `now_unix` (seconds)? A file exactly
/// `max_age` old is still fresh; anything older is stale. Timestamps in the
/// future (clock skew) count as fresh — a future `ts` is never a stale signal,
/// so it must never trigger a respawn.
pub fn is_fresh_at(ts: &str, now_unix: u64, max_age: Duration) -> bool {
    match parse_ts(ts) {
        Some(t) => now_unix.saturating_sub(max_age.as_secs()) <= t,
        None => false,
    }
}

// ---- parsing helpers ----

#[cfg(test)]
mod tests {
    use super::*;

    // ---- path building ----

    #[test]
    fn state_file_path_builds_contract_path() {
        let p = state_file_path(Path::new("/proj"), "agent-1").unwrap();
        assert_eq!(p, PathBuf::from("/proj/.flywheel/agent-state/agent-1.json"));
    }

    #[test]
    fn state_file_path_rejects_empty() {
        assert_eq!(
            state_file_path(Path::new("/proj"), ""),
            Err(NameError::Empty)
        );
    }

    #[test]
    fn state_file_path_rejects_traversal() {
        for bad in ["..", ".", "a/b", "a\\b", "../x", "x/../y"] {
            let r = state_file_path(Path::new("/proj"), bad);
            assert!(
                matches!(r, Err(NameError::Traversal)),
                "name {bad:?} must be rejected as traversal, got {r:?}"
            );
        }
    }

    #[test]
    fn state_file_path_rejects_control_chars() {
        assert!(matches!(
            state_file_path(Path::new("/proj"), "a\u{0}b"),
            Err(NameError::BadChar)
        ));
    }

    #[test]
    fn state_file_path_rejects_embedded_separators() {
        // any separator must fail even when the name looks plausible
        assert!(state_file_path(Path::new("/proj"), "a/b").is_err());
        assert!(state_file_path(Path::new("/proj"), "a\\b").is_err());
    }

    /// bd-context-limit-honor-luqg: the reader must try BOTH identity files
    /// — the herdr pane name AND the spawn-registry mail pin (the contract's
    /// `{AGENT_NAME}` substitutes the pin) — preferring the FRESHER file.
    #[test]
    fn read_state_for_agent_tries_pin_alias_prefers_fresher() {
        let root = std::env::temp_dir().join(format!(
            "fw-agentstate-luqg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir = project_state_dir(&root);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let write = |name: &str, pct: u8, ts: &str| {
            std::fs::write(
                state_file_path(&root, name).unwrap(),
                format!(r#"{{"context_pct": {pct}, "ts": "{ts}"}}"#),
            )
            .unwrap();
        };
        // herdr-name file present, pin file fresher → pin wins.
        write("oc3", 85, "2026-08-22T01:00:00Z");
        write("TanFern", 92, "2026-08-22T01:05:00Z");
        let s = read_state_for_agent(&root, "oc3", Some("TanFern")).expect("some state");
        assert_eq!(s.context_pct, 92, "fresher pin file preferred");
        // pin file absent → herdr-name file still found (no regression).
        write("oc2", 80, "2026-08-22T01:00:00Z");
        let s2 = read_state_for_agent(&root, "oc2", Some("NoSuchPin")).expect("fallback");
        assert_eq!(s2.context_pct, 80);
        // neither identity has a file → None.
        assert!(read_state_for_agent(&root, "ghost", Some("GhostPin")).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- parse ----

    #[test]
    fn parse_state_file_happy() {
        let f = parse_state_file(r#"{ "context_pct": 82, "ts": "2026-08-03T03:00:00Z" }"#).unwrap();
        assert_eq!(f.context_pct, 82);
        assert_eq!(f.ts, "2026-08-03T03:00:00Z");
    }

    #[test]
    fn parse_state_file_malformed_is_none() {
        assert_eq!(parse_state_file("not json"), None);
        assert_eq!(parse_state_file(""), None);
        assert_eq!(parse_state_file(r#"{ "context_pct": 82 }"#), None); // missing ts
        assert_eq!(
            parse_state_file(r#"{ "ts": "2026-08-03T03:00:00Z" }"#),
            None
        ); // missing pct
        assert_eq!(
            parse_state_file(r#"{ "context_pct": 150, "ts": "2026-08-03T03:00:00Z" }"#),
            None // out of range
        );
        assert_eq!(
            parse_state_file(r#"{ "context_pct": 82, "ts": "not-a-time" }"#),
            None // bad ts
        );
    }

    // ---- ts parsing ----

    #[test]
    fn parse_ts_zulu() {
        assert_eq!(parse_ts("2026-08-03T03:00:00Z"), Some(1_785_726_000));
    }

    #[test]
    fn parse_ts_offsets() {
        assert_eq!(parse_ts("2026-08-03T03:00:00+02:00"), Some(1_785_718_800));
        assert_eq!(parse_ts("2026-08-03T03:00:00-05:00"), Some(1_785_744_000));
        assert_eq!(parse_ts("2026-08-03T03:00:00+00:00"), Some(1_785_726_000));
    }

    #[test]
    fn parse_ts_fractional() {
        assert_eq!(parse_ts("2026-08-03T03:00:00.500Z"), Some(1_785_726_000));
    }

    #[test]
    fn parse_ts_rejects_garbage() {
        for bad in [
            "",
            "2026-08-03 03:00:00Z", // space instead of T
            "2026-13-03T03:00:00Z", // month 13
            "2026-08-32T03:00:00Z", // day 32
            "2026-02-29T03:00:00Z", // not a leap year
            "2026-08-03T24:00:00Z", // hour 24
            "2026-08-03T03:00:00",  // no timezone
            "2026-08-03T03:00:00X", // bad tz
            "hello world",
            "2026-08-03T03:00:00+02:00Z", // double tz
        ] {
            assert_eq!(parse_ts(bad), None, "ts {bad:?} must be rejected");
        }
    }

    #[test]
    fn parse_ts_leap_year() {
        // 2024-02-29 is a leap day; 2024 is a leap year
        assert_eq!(parse_ts("2024-02-29T00:00:00Z"), Some(1_709_164_800));
    }

    /// is1.2 edge pins: epoch 0, leap-century boundary (2000 leap by 400-rule,
    /// 2100 not), pre-1970 rejection.
    #[test]
    fn parse_ts_epoch_and_leap_century_boundaries() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        // 2000-02-29 is a leap day (divisible by 400)
        assert_eq!(parse_ts("2000-02-29T00:00:00Z"), Some(951_782_400));
        // 2100-02-29 does not exist (century non-leap) — rejected
        assert_eq!(parse_ts("2100-02-29T00:00:00Z"), None);
        // pre-epoch — the fresh/stale heuristic returns None (never a respawn)
        assert_eq!(parse_ts("1969-12-31T23:59:59Z"), None);
    }

    // ---- freshness ----

    #[test]
    fn freshness_boundary() {
        let ts = "2026-08-03T03:00:00Z"; // unix 1_785_726_000
        let max = Duration::from_mins(15);
        // exactly max_age old → fresh
        assert!(is_fresh_at(ts, 1_785_726_000 + 900, max));
        // one second older → stale
        assert!(!is_fresh_at(ts, 1_785_726_000 + 901, max));
        // much older (previous session) → stale
        assert!(!is_fresh_at(ts, 1_785_726_000 + 3600, max));
        // now == ts → fresh
        assert!(is_fresh_at(ts, 1_785_726_000, max));
        // future ts (clock skew) → fresh, never a stale signal
        assert!(is_fresh_at(ts, 1_785_726_000 - 60, max));
    }

    #[test]
    fn freshness_default_max_age() {
        assert_eq!(DEFAULT_MAX_AGE, Duration::from_mins(15));
    }

    #[test]
    fn freshness_unparseable_is_stale() {
        assert!(!is_fresh_at(
            "garbage",
            1_785_726_000,
            Duration::from_mins(15)
        ));
    }
}

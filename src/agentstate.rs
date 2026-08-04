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
#![allow(dead_code)]

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
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(15 * 60);

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
            NameError::Empty => write!(f, "agent name is empty"),
            NameError::Traversal => write!(f, "agent name contains a path traversal component"),
            NameError::BadChar => write!(f, "agent name contains an unsafe character"),
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

/// Parse an agent state file. Malformed JSON, out-of-range `context_pct`, or an
/// unparseable `ts` all yield `None` — never an error or panic.
pub fn parse_state_file(content: &str) -> Option<AgentStateFile> {
    let f: AgentStateFile = serde_json::from_str(content).ok()?;
    if f.context_pct > 100 {
        return None;
    }
    if parse_ts(&f.ts).is_none() {
        return None;
    }
    Some(f)
}

/// Parse an RFC3339 timestamp to unix seconds.
///
/// Accepts the shapes `state::now_iso` produces (`2026-08-03T03:00:00Z`) plus
/// the common RFC3339 variants: fractional seconds and `±HH:MM` offsets.
/// Any other format → `None` (never panics).
pub fn parse_ts(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let y = parse_int(&bytes[0..4])?;
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let mo = parse_int(&bytes[5..7])?;
    let d = parse_int(&bytes[8..10])?;
    if bytes[10] != b'T' && bytes[10] != b't' {
        return None;
    }
    let h = parse_int(&bytes[11..13])?;
    if bytes[13] != b':' {
        return None;
    }
    let mi = parse_int(&bytes[14..16])?;
    if bytes[16] != b':' {
        return None;
    }
    let s = parse_int(&bytes[17..19])?;

    let mut i = 19;
    // optional fractional seconds (ignored for the unix value)
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    // timezone: Z | z | ±HH:MM
    let mut offset_secs: i64 = 0;
    if i < bytes.len() {
        match bytes[i] {
            b'Z' | b'z' => i += 1,
            b'+' | b'-' => {
                let sign: i64 = if bytes[i] == b'-' { -1 } else { 1 };
                if i + 5 >= bytes.len() || bytes[i + 3] != b':' {
                    return None;
                }
                let oh = parse_int(&bytes[i + 1..i + 3])?;
                let om = parse_int(&bytes[i + 4..i + 6])?;
                if oh > 23 || om > 59 {
                    return None;
                }
                offset_secs = sign * (oh as i64 * 3600 + om as i64 * 60);
                i += 6;
            }
            _ => return None,
        }
    }
    if i != bytes.len() {
        return None;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    if h > 23 || mi > 59 || s > 60 {
        return None;
    }
    if days_in_month(mo, is_leap(y as i64)) < d {
        return None;
    }
    let days = days_from_civil(y as i64, mo, d);
    let unix = days * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64 - offset_secs;
    if unix < 0 {
        return None;
    }
    Some(unix as u64)
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

/// `is_fresh_at` with the wall clock.
pub fn is_fresh(ts: &str, max_age: Duration) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    is_fresh_at(ts, now, max_age)
}

// ---- parsing helpers ----

fn parse_int(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > 4 {
        return None;
    }
    let mut v: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u32;
    }
    Some(v)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(m: u32, leap: bool) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Hinnant days-from-civil: civil (y, m, d) → days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as u32; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    (era * 146097 + doe as i64) - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- path building ----

    #[test]
    fn state_file_path_builds_contract_path() {
        let p = state_file_path(Path::new("/proj"), "agent-1").unwrap();
        assert_eq!(
            p,
            PathBuf::from("/proj/.flywheel/agent-state/agent-1.json")
        );
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

    // ---- parse ----

    #[test]
    fn parse_state_file_happy() {
        let f = parse_state_file(r#"{ "context_pct": 82, "ts": "2026-08-03T03:00:00Z" }"#)
            .unwrap();
        assert_eq!(f.context_pct, 82);
        assert_eq!(f.ts, "2026-08-03T03:00:00Z");
    }

    #[test]
    fn parse_state_file_malformed_is_none() {
        assert_eq!(parse_state_file("not json"), None);
        assert_eq!(parse_state_file(""), None);
        assert_eq!(parse_state_file(r#"{ "context_pct": 82 }"#), None); // missing ts
        assert_eq!(parse_state_file(r#"{ "ts": "2026-08-03T03:00:00Z" }"#), None); // missing pct
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

    // ---- freshness ----

    #[test]
    fn freshness_boundary() {
        let ts = "2026-08-03T03:00:00Z"; // unix 1_785_726_000
        let max = Duration::from_secs(15 * 60);
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
        assert_eq!(DEFAULT_MAX_AGE, Duration::from_secs(900));
    }

    #[test]
    fn freshness_unparseable_is_stale() {
        assert!(!is_fresh_at("garbage", 1_785_726_000, Duration::from_secs(900)));
    }
}

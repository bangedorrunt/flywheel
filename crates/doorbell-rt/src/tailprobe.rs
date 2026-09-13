// governed-by: ADR-0012
//! Bounded tail probe over the append-only surfaces (`{ws}.audit.jsonl`,
//! `doorbell-journal-{ws}.jsonl`).
//!
//! Why this exists: `/health` must answer "why didn't I get woken?" from
//! the ground truth the plane already writes. Live (2026-09-13) a daemon
//! whose every inbox fetch failed — an `inbox_error` audit row per pin per
//! second, plus a `RestartWatch` every 16s — reported `degraded: []`, so
//! the outage was invisible in the one place an operator looks. Reading
//! the newest matching line's age turns those facts into the health signal
//! they already were.
//!
//! Bounded by contract: only the last [`MAX_TAIL_BYTES`] are read, so a
//! 10 MiB audit file costs the same as an empty one, and a line longer
//! than the window is simply not seen (the probes look for a marker plus a
//! timestamp, and appends are small).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the tail is examined. Audit rows are ~150 B; the journal
/// emits one sweep row per second (~200 B), so a 300 s counting window
/// needs ~60 KiB and this leaves generous headroom. Still bounded: a
/// 10 MiB audit file costs the same as an empty one.
pub const MAX_TAIL_BYTES: u64 = 256 * 1024;

/// Age in seconds of the NEWEST line in `path` that contains `needle` and
/// carries an RFC3339 `"ts"` field, relative to `now`. `None` = no such
/// line in the examined tail (never seen, or older than the window).
pub fn newest_match_age_secs(path: &Path, needle: &str, now: u64) -> Option<u64> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    f.take(MAX_TAIL_BYTES).read_to_string(&mut buf).ok()?;
    // Walking backwards finds the newest match first; the first line of a
    // mid-file window may be torn, and a torn line simply fails to parse.
    for line in buf.lines().rev() {
        if !line.contains(needle) {
            continue;
        }
        let Some(ts) = crate::tailprobe::ts_of(line) else {
            continue;
        };
        return Some(now.saturating_sub(ts));
    }
    None
}

/// Extract the unix seconds of a JSON line's `"ts"` (RFC3339, the shape
/// both audit and journal writers emit). Hand-parsed to avoid a second
/// JSON pass over lines that already proved interesting by substring.
fn ts_of(line: &str) -> Option<u64> {
    let after = line.split("\"ts\":\"").nth(1)?;
    let stamp = after.split('"').next()?;
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|dt| dt.timestamp().max(0) as u64)
}

/// Newest matching age across several files (min age = most recent).
/// `None` only when no file has a match in its window.
pub fn newest_match_age_secs_across<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    needle: &str,
    now: u64,
) -> Option<u64> {
    paths
        .into_iter()
        .filter_map(|p| newest_match_age_secs(p, needle, now))
        .min()
}

/// Count matching lines inside the examined tail whose `ts` is within
/// `window_secs` of `now`.
///
/// Why a count and not an age: a healthy `toron watch` child idle-exits by
/// design (90 s idle + a retry gate), so individual restarts are ROUTINE and
/// two hosted workspaces phase them independently — a "most recent restart"
/// probe fires on a healthy plane roughly half the time. Thrash is a
/// *frequency* (live: every 16 s), so it is measured as a frequency.
pub fn count_matches_within(path: &Path, needle: &str, now: u64, window_secs: u64) -> usize {
    let Ok(mut f) = std::fs::File::open(path) else {
        return 0;
    };
    let Ok(md) = f.metadata() else { return 0 };
    let start = md.len().saturating_sub(MAX_TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return 0;
    }
    let mut buf = String::new();
    if f.take(MAX_TAIL_BYTES).read_to_string(&mut buf).is_err() {
        return 0;
    }
    buf.lines()
        .filter(|line| line.contains(needle))
        .filter_map(ts_of)
        .filter(|ts| now.saturating_sub(*ts) <= window_secs)
        .count()
}

/// Sum of [`count_matches_within`] across several files.
pub fn count_matches_within_across<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    needle: &str,
    now: u64,
    window_secs: u64,
) -> usize {
    paths
        .into_iter()
        .map(|p| count_matches_within(p, needle, now, window_secs))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dbrt-tail-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn row(ts: &str, kind: &str) -> String {
        format!("{{\"ts\":\"{ts}\",\"kind\":\"{kind}\",\"workspace\":\"w\"}}\n")
    }

    #[test]
    fn newest_matching_line_wins() {
        let dir = scratch("newest");
        let p = dir.join("w.audit.jsonl");
        fs::write(
            &p,
            format!(
                "{}{}{}",
                row("2026-09-13T07:00:00Z", "inbox_error"),
                row("2026-09-13T07:22:00Z", "budget_drop"),
                row("2026-09-13T07:22:53Z", "inbox_error"),
            ),
        )
        .expect("write");
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-13T07:23:00Z")
            .unwrap()
            .timestamp() as u64;
        assert_eq!(
            newest_match_age_secs(&p, "\"kind\":\"inbox_error\"", now),
            Some(7),
            "the newest inbox_error row sets the age, not the oldest"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_match_is_none_and_missing_file_is_none() {
        let dir = scratch("none");
        let p = dir.join("w.audit.jsonl");
        fs::write(&p, row("2026-09-13T07:22:00Z", "budget_drop")).expect("write");
        let now = 1_800_000_000;
        assert_eq!(newest_match_age_secs(&p, "inbox_error", now), None);
        assert_eq!(
            newest_match_age_secs(&dir.join("absent.jsonl"), "inbox_error", now),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The probe must read the TAIL: a huge prefix of old errors never
    /// hides a fresh one, and the read stays bounded.
    #[test]
    fn tail_window_sees_fresh_row_past_a_large_prefix() {
        let dir = scratch("window");
        let p = dir.join("w.audit.jsonl");
        let mut body = String::new();
        for _ in 0..2000 {
            body.push_str(&row("2026-09-13T06:00:00Z", "inbox_error"));
        }
        body.push_str(&row("2026-09-13T07:22:53Z", "inbox_error"));
        fs::write(&p, body).expect("write");
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-13T07:23:00Z")
            .unwrap()
            .timestamp() as u64;
        assert_eq!(
            newest_match_age_secs(&p, "inbox_error", now),
            Some(7),
            "a fresh row inside the tail window is found"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A healthy plane restarts each watcher ~every 109 s; the broken one
    /// every 16 s. Counting inside a window separates them where a
    /// most-recent-restart probe cannot.
    #[test]
    fn count_within_window_separates_healthy_cadence_from_thrash() {
        let dir = scratch("count");
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-13T07:30:00Z")
            .unwrap()
            .timestamp() as u64;
        let at = |offset: i64| {
            chrono::DateTime::from_timestamp(now as i64 + offset, 0)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        };
        // healthy: restarts 109 s apart, both inside a 300 s window
        let healthy = dir.join("healthy.jsonl");
        let mut body = String::new();
        for off in [-218, -109, 0] {
            body.push_str(&format!(
                "{{\"ts\":\"{}\",\"actions\":[\"RestartWatch\"]}}\n",
                at(off)
            ));
            // sweep rows are the bulk of a real journal
            body.push_str(&format!(
                "{{\"ts\":\"{}\",\"actions\":[\"FetchInbox\"]}}\n",
                at(off)
            ));
        }
        fs::write(&healthy, body).expect("write healthy");
        assert_eq!(count_matches_within(&healthy, "RestartWatch", now, 300), 3);
        // thrash: every 16 s
        let thrash = dir.join("thrash.jsonl");
        let mut body = String::new();
        for off in (0..=288).step_by(16) {
            body.push_str(&format!(
                "{{\"ts\":\"{}\",\"actions\":[\"RestartWatch\"]}}\n",
                at(-off)
            ));
        }
        fs::write(&thrash, body).expect("write thrash");
        assert_eq!(count_matches_within(&thrash, "RestartWatch", now, 300), 19);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn count_outside_the_window_is_not_counted() {
        let dir = scratch("count-window");
        let now = 1_800_000_000;
        let old = chrono::DateTime::from_timestamp(now as i64 - 10_000, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let p = dir.join("w.jsonl");
        fs::write(
            &p,
            format!("{{\"ts\":\"{old}\",\"actions\":[\"RestartWatch\"]}}\n"),
        )
        .expect("write");
        assert_eq!(count_matches_within(&p, "RestartWatch", now, 300), 0);
        assert_eq!(count_matches_within(&dir.join("absent"), "x", now, 300), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn across_files_takes_the_most_recent() {
        let dir = scratch("across");
        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        fs::write(&a, row("2026-09-13T07:00:00Z", "inbox_error")).expect("write a");
        fs::write(&b, row("2026-09-13T07:22:53Z", "inbox_error")).expect("write b");
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-13T07:23:00Z")
            .unwrap()
            .timestamp() as u64;
        assert_eq!(
            newest_match_age_secs_across([a.as_path(), b.as_path()], "inbox_error", now),
            Some(7)
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

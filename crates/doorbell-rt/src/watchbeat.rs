// governed-by: ADR-0012
//! Watch-delivery stamps (GS-062 honest split):
//! - **Event** file `{state}/doorbell/watch-{workspace}.event` — stamped when a
//!   real NDJSON watch line arrives (`last_event`).
//! - **Heartbeat** file `{state}/doorbell/watch-{workspace}.hb` — stamped on a
//!   periodic 1s tick while the watch child is alive (even with no lines).
//!
//! Never pass `max(event, hb)` into `last_event_ms` (wake_liveness classifier
//! tests encode this). `/health` reads both stamps separately.
//!
//! Why this exists: a watcher pointed at the wrong bus (mailbox env mismatch)
//! or a dead child is *indistinguishable from a quiet workspace* by every other
//! signal. These files are the honest answer to "is the bus delivering to THIS
//! workspace?".

use std::path::{Path, PathBuf};

/// `{state}/doorbell/watch-{workspace}.hb` (heartbeat only).
pub fn path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("watch-{workspace}.hb"))
}

/// `{state}/doorbell/watch-{workspace}.event` (event lines only).
pub fn event_path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("watch-{workspace}.event"))
}

fn stamp_file(p: &Path) {
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(p, crate::lease::unix_now().to_string());
}

/// Stamp the **event** file (real watch line arrived). Best-effort.
pub fn touch_event(state_dir: &Path, workspace: &str) {
    stamp_file(&event_path(state_dir, workspace));
}

/// Stamp the **heartbeat** file (periodic tick while watch child alive).
pub fn touch_heartbeat(state_dir: &Path, workspace: &str) {
    stamp_file(&path(state_dir, workspace));
}

/// Back-compat alias: historically conflated event+hb into `.hb`.
/// GS-062: aliases to [`touch_heartbeat`] so callers that meant "pipe alive"
/// stay honest; prefer [`touch_event`] / [`touch_heartbeat`] explicitly.
pub fn touch(state_dir: &Path, workspace: &str) {
    touch_heartbeat(state_dir, workspace);
}

/// Absolute mtime of a stamp file in unix **milliseconds**, or `None`.
pub fn stamp_ms(path: &Path) -> Option<u64> {
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(mtime.as_millis() as u64)
}

/// Last event stamp ms (None = never stamped).
pub fn last_event_ms(state_dir: &Path, workspace: &str) -> Option<u64> {
    stamp_ms(&event_path(state_dir, workspace))
}

/// Last heartbeat stamp ms (None = never stamped).
pub fn last_heartbeat_ms(state_dir: &Path, workspace: &str) -> Option<u64> {
    stamp_ms(&path(state_dir, workspace))
}

/// Age of the last **heartbeat** in whole seconds, or `None` when never stamped.
pub fn age_secs(state_dir: &Path, workspace: &str) -> Option<u64> {
    file_age_secs(&path(state_dir, workspace))
}

/// Age of the last **event** in whole seconds, or `None` when never stamped.
pub fn event_age_secs(state_dir: &Path, workspace: &str) -> Option<u64> {
    file_age_secs(&event_path(state_dir, workspace))
}

/// mtime age of an arbitrary stamped file, `None` when missing/unreadable.
pub fn file_age_secs(path: &Path) -> Option<u64> {
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(crate::lease::unix_now().saturating_sub(mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbrt-watchbeat-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn unstamped_is_unknown_not_zero() {
        let d = dir("unknown");
        assert_eq!(age_secs(&d, "w"), None);
        assert_eq!(event_age_secs(&d, "w"), None);
        assert_eq!(last_event_ms(&d, "w"), None);
        assert_eq!(last_heartbeat_ms(&d, "w"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn event_and_hb_are_distinct_stamps() {
        let d = dir("split");
        touch_event(&d, "w");
        assert!(last_event_ms(&d, "w").is_some());
        assert!(
            last_heartbeat_ms(&d, "w").is_none(),
            "event must not stamp hb"
        );
        touch_heartbeat(&d, "w");
        assert!(last_heartbeat_ms(&d, "w").is_some());
        assert_eq!(
            event_path(&d, "w"),
            d.join("doorbell").join("watch-w.event")
        );
        assert_eq!(path(&d, "w"), d.join("doorbell").join("watch-w.hb"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stamp_round_trips_and_is_per_workspace() {
        let d = dir("roundtrip");
        touch_heartbeat(&d, "w");
        assert!(age_secs(&d, "w").is_some_and(|a| a < 5));
        assert_eq!(age_secs(&d, "other"), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}

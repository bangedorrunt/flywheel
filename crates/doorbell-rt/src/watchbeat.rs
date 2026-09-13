// governed-by: ADR-0012
//! Watch-delivery heartbeat: the host stamps
//! `{state}/doorbell/watch-{workspace}.hb` on every NDJSON line its
//! `toron watch --live` child emits, and `/health` reads the mtime back as
//! `watch.last_event_age_s`.
//!
//! Why this exists: a watcher pointed at the wrong bus (mailbox env
//! mismatch) or a dead child is *indistinguishable from a quiet workspace*
//! by every other signal — the sweep heartbeat stays fresh, the journal
//! keeps ticking, `mode` says `hosted`, and no ring ever fires. This file
//! is the one honest answer to "is the bus delivering to THIS workspace?",
//! so an operator can see deafness instead of inferring it from silence.

use std::path::{Path, PathBuf};

/// `{state}/doorbell/watch-{workspace}.hb`.
pub fn path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("watch-{workspace}.hb"))
}

/// Stamp the heartbeat now. Best-effort by contract: a missed stamp never
/// breaks the watch path, it only makes `/health` report a staler age.
pub fn touch(state_dir: &Path, workspace: &str) {
    let p = path(state_dir, workspace);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, crate::lease::unix_now().to_string());
}

/// Age of the last stamped event in whole seconds, or `None` when this
/// workspace has never seen one (never stamped, or unreadable) — `None` is
/// UNKNOWN, never `0`, so a dead watch can't read as a fresh one.
pub fn age_secs(state_dir: &Path, workspace: &str) -> Option<u64> {
    file_age_secs(&path(state_dir, workspace))
}

/// mtime age of an arbitrary stamped file, `None` when missing/unreadable.
/// Shared with `/health`'s presence-projection probe.
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
        assert_eq!(
            age_secs(&d, "w"),
            None,
            "never stamped must read UNKNOWN, never a fresh-looking 0"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stamp_round_trips_and_is_per_workspace() {
        let d = dir("roundtrip");
        touch(&d, "w");
        assert!(
            age_secs(&d, "w").is_some_and(|a| a < 5),
            "fresh stamp reads fresh"
        );
        assert_eq!(
            age_secs(&d, "other"),
            None,
            "workspaces never share a heartbeat"
        );
        assert_eq!(
            path(&d, "w"),
            d.join("doorbell").join("watch-w.hb"),
            "path shape is the /health contract"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}

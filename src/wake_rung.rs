//! One atomic claim for one bus event id (bd-tdpix).
//!
//! Every delivery path — doorbell, opencode stdio proxy, pi extension — claims
//! the same per-id marker under `<state>/wake-rung/`. The marker is created with
//! O_CREAT|O_EXCL, which is atomic across processes AND languages (node's
//! `fs.openSync(path, 'wx')`), so exactly one claimer rings. A bounded age
//! sweep keeps the directory from growing forever. No JSONL read-modify-write.

use std::path::{Path, PathBuf};

pub const RUNG_DIR: &str = "wake-rung";
/// The ONE marker TTL. Exported to the TS side via FLYWHEEL_WAKE_RUNG_TTL_SECS
/// so no language sweeps another's marker early (bd-tdpix review).
pub const WAKE_RUNG_TTL_SECS: u64 = 86_400;

fn marker_path(state_dir: &Path, event_id: &str) -> PathBuf {
    let mut safe: String = event_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if safe.is_empty() {
        safe.push_str("empty");
    }
    state_dir.join(RUNG_DIR).join(format!("{safe}.rung"))
}

/// True only for the FIRST claimer of `event_id`. `create_new` is
/// O_CREAT|O_EXCL: a racing writer gets AlreadyExists and loses the claim.
pub fn claim(state_dir: &Path, event_id: &str) -> bool {
    let dir = state_dir.join(RUNG_DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker_path(state_dir, event_id))
        .is_ok()
}

/// Remove markers older than `max_age_secs` (best effort), returning how many.
pub fn sweep(state_dir: &Path, max_age_secs: u64, now_secs: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(state_dir.join(RUNG_DIR)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let age = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now_secs.saturating_sub(age) > max_age_secs && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn n_racing_claimers_produce_exactly_one_winner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let winners = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..12 {
                scope.spawn(|| {
                    if claim(dir.path(), "bus-event-42") {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(winners.load(Ordering::SeqCst), 1, "one ring per event id");
    }

    #[test]
    fn a_second_claim_on_a_different_id_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(claim(dir.path(), "e1"));
        assert!(!claim(dir.path(), "e1"));
        assert!(claim(dir.path(), "e2"));
    }

    #[test]
    fn the_sweep_drops_old_markers_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(claim(dir.path(), "old"));
        assert!(claim(dir.path(), "fresh"));
        // now far beyond the markers' mtime: both older than max_age.
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        assert_eq!(sweep(dir.path(), 60, future), 2);
        assert_eq!(
            std::fs::read_dir(dir.path().join(RUNG_DIR))
                .unwrap()
                .count(),
            0
        );
    }
}

//! Installed-binary staleness (bd-xaw8): the tier-aware harness menu was
//! invisible for 16.5h because the installed binary predated its wiring.
//! Mirrors the doorbell `stale_build` health field: compare the running
//! executable's mtime against the newest source mtime under `src/`.
//! Honestly tri-state: stale and fresh are proven, unknown (unresolvable
//! paths) serializes as absent, never as a false claim either way.

use std::path::{Path, PathBuf};

/// Staleness verdict. Unknown renders as absent downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
}

/// Pure core: decide from two optional mtimes. Missing either side is
/// Unknown, never a guess.
pub fn freshness_of(exe_mtime_s: Option<u64>, src_mtime_s: Option<u64>) -> Freshness {
    match (exe_mtime_s, src_mtime_s) {
        (Some(exe), Some(src)) => {
            if exe < src {
                Freshness::Stale
            } else {
                Freshness::Fresh
            }
        }
        _ => Freshness::Unknown,
    }
}

/// Newest mtime (unix secs) under a directory, non-recursive walk like the
/// doorbell probe (work-list, no helper to test separately).
pub fn newest_mtime_under(dir: &Path) -> Option<u64> {
    let mut newest: Option<u64> = None;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let t = std::fs::metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            newest = newest.max(t);
        }
    }
    newest
}

/// Running executable mtime (unix secs). None when unresolvable.
pub fn exe_mtime() -> Option<u64> {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Sources root for the installed binary: the checkout this binary was built
/// from. Same assumption as the doorbell probe (operator builds on this
/// machine); unknown elsewhere rather than wrong.
pub fn sources_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    root.is_dir().then_some(root)
}

/// Full verdict for this process.
pub fn freshness() -> Freshness {
    let src = sources_root().and_then(|r| newest_mtime_under(&r));
    freshness_of(exe_mtime(), src)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-xaw8: the pure core decides on numbers alone — no filesystem.
    #[test]
    fn freshness_of_decides_without_guessing() {
        assert_eq!(freshness_of(Some(10), Some(5)), Freshness::Fresh);
        assert_eq!(freshness_of(Some(5), Some(10)), Freshness::Stale);
        assert_eq!(freshness_of(Some(5), Some(5)), Freshness::Fresh);
        assert_eq!(freshness_of(None, Some(5)), Freshness::Unknown);
        assert_eq!(freshness_of(Some(5), None), Freshness::Unknown);
        assert_eq!(freshness_of(None, None), Freshness::Unknown);
    }

    /// bd-xaw8: the walk finds the newest .rs and ignores the rest.
    /// Hermetic temp dirs; works for any uid.
    #[test]
    fn newest_mtime_finds_rs_only() {
        let dir = std::env::temp_dir().join(format!("fw-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("old.rs"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        // Newest .rs must win over older files of any kind.
        let t0 = newest_mtime_under(&dir).expect("finds old.rs");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.join("sub").join("new.rs"), b"x").unwrap();
        let t1 = newest_mtime_under(&dir).expect("finds new.rs");
        assert!(t1 >= t0, "newer file must not read older: {t0} -> {t1}");
        // A tree with no .rs is Unknown, not epoch-zero.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("only.txt"), b"x").unwrap();
        assert_eq!(newest_mtime_under(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

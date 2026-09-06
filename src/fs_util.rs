//! Shared atomic file-write helper (l4s.3).
//!
//! Atomic write = temp file in the SAME directory + `fs::rename`. A crash
//! mid-write leaves only the temp file, never a torn destination. This is
//! the pattern state.rs already uses for loops sidecars / last_sent; the
//! third use (crons.toml) justified extracting it so the discipline is
//! enforced, not copied.
//!
//! governed-by: ADR-0003

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Write `bytes` to `path` atomically: write a sibling temp file, fsync it,
/// then rename over the destination. The temp name is `.<file>.tmp-<pid>` —
/// unique per process, same filesystem (rename is atomic on POSIX).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("atomic_write: {} has no parent dir", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("atomic_write: {} has no file name", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let res = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create temp {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write temp {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("sync temp {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    })();
    if res.is_err() {
        // Best-effort cleanup of the temp file on failure.
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_roundtrips_and_overwrites() {
        let path =
            std::env::temp_dir().join(format!("flywheel-atomic-write-{}", std::process::id()));
        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir =
            std::env::temp_dir().join(format!("flywheel-atomic-dir-{}/nested", std::process::id()));
        let path = dir.join("data.toml");
        atomic_write(&path, b"x").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("flywheel-atomic-dir-{}", std::process::id())),
        );
    }

    #[test]
    fn atomic_write_failure_leaves_no_partial_destination() {
        // Parent is a FILE → create_dir_all fails → error, and the temp is
        // cleaned up.
        let dir = std::env::temp_dir().join(format!("flywheel-atomic-fail-{}", std::process::id()));
        std::fs::write(&dir, b"not-a-dir").unwrap();
        let path = dir.join("x.toml");
        let err = atomic_write(&path, b"data").expect_err("write must fail");
        assert!(!format!("{err:#}").is_empty());
        assert!(!path.exists(), "no destination written on failure");
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".x.toml.tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp cleaned up on failure");
        let _ = std::fs::remove_file(&dir);
    }
}

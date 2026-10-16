//! Owner-only files and directories (0600 / 0700) for state that can carry
//! steer content, ring budgets or lease holders. Unix modes; a no-op shape
//! elsewhere (the std calls still create the files).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Create `dir` and every missing parent as 0700, and tighten `dir` itself if
/// it already exists with looser bits.
pub fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Create (or truncate) `path` as 0600. An existing file with looser bits is
/// tightened too.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    open_private(OpenOptions::new().write(true).create(true).truncate(true), path)
}

/// Open `path` for append as 0600, creating it if absent.
pub fn open_private_append(path: &Path) -> io::Result<File> {
    open_private(OpenOptions::new().append(true).create(true), path)
}

fn open_private(o: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let f = o.mode(0o600).open(path)?;
        let mode = f.metadata()?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(f)
    }
    #[cfg(not(unix))]
    {
        o.open(path)
    }
}

/// Write `bytes` to `path` as a 0600 file, atomically (temp file in the same
/// directory, then rename), so a reader never sees a half-written or
/// world-readable file.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = create_private_file(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path)
}

/// The Unix mode bits of `path` (0 elsewhere).
pub fn mode_of(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777).unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn dirs_and_files_are_created_owner_only_whatever_the_umask() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("a/b/c");
        create_private_dir_all(&deep).unwrap();
        for p in [d.path().join("a"), d.path().join("a/b"), deep.clone()] {
            assert_eq!(mode_of(&p), 0o700, "{p:?}");
        }
        let mut f = create_private_file(&deep.join("f")).unwrap();
        f.write_all(b"x").unwrap();
        assert_eq!(mode_of(&deep.join("f")), 0o600);
        let mut g = open_private_append(&deep.join("g")).unwrap();
        g.write_all(b"y").unwrap();
        assert_eq!(mode_of(&deep.join("g")), 0o600);
    }

    #[test]
    fn existing_loose_modes_are_tightened() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("s");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let f = dir.join("f");
        std::fs::write(&f, b"old").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        create_private_dir_all(&dir).unwrap();
        create_private_file(&f).unwrap();
        assert_eq!((mode_of(&dir), mode_of(&f)), (0o700, 0o600));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
        drop(open_private_append(&f).unwrap());
        assert_eq!(mode_of(&f), 0o600);
    }

    #[test]
    fn atomic_write_leaves_a_private_complete_file_and_no_temp() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.json");
        write_private_atomic(&p, b"one").unwrap();
        write_private_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(mode_of(&p), 0o600);
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "no temp left behind");
    }
}

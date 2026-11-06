// Test-only helper: fake rex binary guard. Mirrors toron_core's RexBinGuard.
#[cfg(test)]
pub struct RexBinGuard {
    _dir: tempfile::TempDir,
    log: std::path::PathBuf,
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
static REX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
impl RexBinGuard {
    pub fn install(script: &str) -> Self {
        let _lock = REX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("log");
        let script = script.replace("@@LOG@@", &shell_quote(&log));
        let bin = dir.path().join("rex");
        write_exec(&bin, &script);
        let prev = std::env::var_os("REX_BIN_DIR");
        unsafe { std::env::set_var("REX_BIN_DIR", dir.path()) };
        Self {
            _dir: dir,
            log,
            prev,
            _lock,
        }
    }

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[cfg(test)]
impl Drop for RexBinGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev {
                Some(value) => std::env::set_var("REX_BIN_DIR", value),
                None => std::env::remove_var("REX_BIN_DIR"),
            }
        }
    }
}

#[cfg(test)]
fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

#[cfg(test)]
fn write_exec(path: &std::path::Path, script: &str) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::File::create(path).expect("create rex");
    file.write_all(script.as_bytes()).expect("write rex");
    let mut perms = file.metadata().expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

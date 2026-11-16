//! One sandbox for every test that starts a flywheel/toron/doorbell process or
//! touches the supervisor.
//!
//! Contract (AGENTS.md "Test sandbox"): a temp HOME plus every XDG dir and
//! TORON_DATA_DIR, the operator's live routing env scrubbed, deny-shims for
//! `launchctl`/`systemctl` first on PATH, and a Drop that asserts no shim was
//! reached. A test that MEANS to exercise the shim opts in with
//! [`TestSandbox::allow_supervisor_calls`].
//!
//! Two shapes, because tests spawn in two ways:
//! * [`TestSandbox::command`] — a child process, `.env_clear()`ed and rebuilt
//!   from the sandbox only (a stray live var cannot reach it).
//! * [`TestSandbox::scope`] — an in-process test that shells out itself; the
//!   process env is swapped under a global lock and restored on drop.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// The operator's live routing env: never inherited by a sandboxed child.
pub const SCRUBBED: &[&str] = &[
    "TORON_MCP_URL",
    "FLYWHEEL_TORON_URL",
    "TORON_URL",
    "REX_SERVER",
    "REX_SESSION",
    "REX_BLOCK",
    "HC_SOCKET_PATH",
    "HC_TERMINAL_ID",
    "AGENT_NAME",
];

const SUPERVISOR_SHIMS: [&str; 2] = ["launchctl", "systemctl"];

pub struct TestSandbox {
    dir: tempfile::TempDir,
    allow_supervisor: AtomicBool,
}

impl TestSandbox {
    #[must_use]
    pub fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("sandbox temp dir");
        for sub in [
            "home", "config", "state", "data", "toron", "bin", "src", "cargo",
        ] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("sandbox dir");
        }
        std::fs::write(dir.path().join("src/Cargo.toml"), "").expect("dummy Cargo.toml");
        let log = dir.path().join("shim.log");
        for tool in SUPERVISOR_SHIMS {
            let path = dir.path().join("bin").join(tool);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\necho \"{tool} $*\" >> \"{}\"\nexit 1\n",
                    log.display()
                ),
            )
            .expect("shim");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("shim mode");
            }
        }
        Self {
            dir,
            allow_supervisor: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The deny-shim dir, first on every PATH this sandbox hands out. A test
    /// may drop its own fake tools here (they shadow the real ones).
    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.root().join("bin")
    }

    /// `<shims>:<inherited PATH>` for an in-process test that shells out.
    #[must_use]
    pub fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.bin_dir().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    #[must_use]
    pub fn shim_log(&self) -> String {
        std::fs::read_to_string(self.root().join("shim.log")).unwrap_or_default()
    }

    /// Opt out of the Drop assertion: this test WANTS the deny-shim to run.
    pub fn allow_supervisor_calls(&self) {
        self.allow_supervisor.store(true, Ordering::SeqCst);
    }

    /// A child process with nothing inherited: `.env_clear()` plus the sandbox.
    #[must_use]
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", self.bin_dir())
            .env("HOME", self.root().join("home"))
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_STATE_HOME", self.root().join("state"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_RUNTIME_DIR", self.root().join("state"))
            .env("TORON_DATA_DIR", self.root().join("toron"))
            .env("TORON_MCP_DATA_DIR", self.root().join("toron"))
            .env("FLYWHEEL_SOURCE", self.root().join("src"))
            .env("CARGO_INSTALL_ROOT", self.root().join("cargo"));
        cmd
    }

    /// Swap the process env for an in-process test; restored on drop.
    #[must_use]
    pub fn scope(&self) -> SandboxScope {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut saved: Vec<(String, Option<OsString>)> = Vec::new();
        let mut set = |key: &str, value: OsString| {
            saved.push((key.to_string(), std::env::var_os(key)));
            unsafe { std::env::set_var(key, value) };
        };
        set("PATH", self.path_env().into());
        set("HOME", self.root().join("home").into());
        set("XDG_CONFIG_HOME", self.root().join("config").into());
        set("XDG_STATE_HOME", self.root().join("state").into());
        set("XDG_DATA_HOME", self.root().join("data").into());
        set("XDG_RUNTIME_DIR", self.root().join("state").into());
        set("TORON_DATA_DIR", self.root().join("toron").into());
        set("TORON_MCP_DATA_DIR", self.root().join("toron").into());
        for key in SCRUBBED {
            saved.push(((*key).to_string(), std::env::var_os(key)));
            unsafe { std::env::remove_var(key) };
        }
        SandboxScope {
            saved,
            _lock: guard,
        }
    }
}

impl Default for TestSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestSandbox {
    fn drop(&mut self) {
        let log = self.shim_log();
        assert!(
            self.allow_supervisor.load(Ordering::SeqCst) || log.is_empty(),
            "a sandboxed test reached the real supervisor (deny-shim log: {log}); \
             the live launchctl/systemctl must never run from a test"
        );
    }
}

pub struct SandboxScope {
    saved: Vec<(String, Option<OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for SandboxScope {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                Some(v) => unsafe { std::env::set_var(&key, v) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

// governed-by: ADR-0012
//! Daemon lifecycle (D2): paths, pidfile, spawn, verify, stop, status.
//! Units ship `Restart=on-failure` (a clean exit over a port conflict must
//! STAY dead — the bd-serve-double-spawn lesson); no `always`.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 18980;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    pub port: u16,
    /// Flywheel state dir (journals, leases, registries live here —
    /// shared with embedded loops). Mirrors flywheel's own resolution:
    /// `DOORBELL_FLYWHEEL_STATE_DIR` > `XDG_STATE_HOME/flywheel` >
    /// platform state dir + `flywheel`.
    pub flywheel_state: PathBuf,
    /// Hosted loops as `(project, workspace)` pairs (`--host p/w`,
    /// repeatable). Empty = standby: bind + reads only.
    pub hosts: Vec<(String, String)>,
}

/// Parse one `--host project/workspace` value.
/// Split `--host <project>/<workspace>` on the LAST `/`.
///
/// `rsplit_once` is load-bearing, not style: project keys are routinely
/// ABSOLUTE paths (`/tmp/scratch/proj`), and splitting on the first `/`
/// yields an empty project and bails — so every harness/scratch project
/// was unhostable (`doorbell serve --host /abs/proj/ws` refused), which is
/// exactly the workspaces that need a hosted ring most.
pub fn parse_host(s: &str) -> anyhow::Result<(String, String)> {
    match s.rsplit_once('/') {
        Some((p, w)) if !p.is_empty() && !w.is_empty() => Ok((p.to_string(), w.to_string())),
        _ => anyhow::bail!("--host must be project/workspace, got {s:?}"),
    }
}

/// Default daemon state dir (bd-fpd5): `DOORBELL_STATE_DIR` >
/// `XDG_STATE_HOME/doorbell` > `~/.local/state/doorbell`. Same ladder
/// shape as [`default_flywheel_state`], except the last rung is the
/// portable home fallback, never the platform data dir — the daemon
/// state converges to XDG on every platform.
///
/// The pure ladder lives in [`resolve_state_dir_default`] (no env reads)
/// so unit tests exercise it hermetically; this wrapper only reads env.
pub fn default_state_dir() -> PathBuf {
    resolve_state_dir_default(
        std::env::var_os("DOORBELL_STATE_DIR").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

pub fn resolve_state_dir_default(
    env_doorbell: Option<PathBuf>,
    env_xdg: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    env_doorbell
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            env_xdg
                .filter(|p| !p.as_os_str().is_empty())
                .map(|d| d.join("doorbell"))
        })
        .unwrap_or_else(|| {
            home.unwrap_or_else(|| PathBuf::from("~"))
                .join(".local/state/doorbell")
        })
}

/// Pre-XDG default we migrate away from (`dirs::data_dir/doorbell`, i.e.
/// `~/Library/Application Support/doorbell` on macOS). `None` when the
/// platform has no data dir at all.
pub fn legacy_platform_state_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("doorbell"))
}

/// Migrate-once from the legacy platform dir into the default state dir
/// (bd-fpd5). Copies via [`crate::migrate::migrate`] (byte-identical,
/// refuse-overwrite) when the old dir exists and the new one carries no
/// inventory yet; every other shape is a clean `None` no-op (same dir,
/// absent old dir, already-migrated or user-managed new dir). Returns
/// the copied relative paths when a migration ran.
pub fn maybe_migrate_to_default(
    new: &std::path::Path,
    old: &std::path::Path,
) -> Result<Option<Vec<String>>> {
    if new == old {
        return Ok(None);
    }
    if !old.is_dir() {
        return Ok(None);
    }
    if dir_has_inventory(new) {
        return Ok(None);
    }
    let copied = crate::migrate::migrate(old, new)?;
    Ok(Some(copied))
}

fn dir_has_inventory(dir: &std::path::Path) -> bool {
    for sub in ["doorbell", "journals", ""] {
        let d = if sub.is_empty() {
            dir.to_path_buf()
        } else {
            dir.join(sub)
        };
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(true) {
                continue;
            }
            if crate::migrate::inventory_match(sub, &name) {
                return true;
            }
        }
    }
    false
}

/// True when the env var is unset or empty (an explicit override must be
/// non-empty to count).
pub fn env_is_unset_or_empty(name: &str) -> bool {
    std::env::var_os(name).map(|v| v.is_empty()).unwrap_or(true)
}

pub fn default_flywheel_state() -> PathBuf {
    std::env::var_os("DOORBELL_FLYWHEEL_STATE_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .map(|d| d.join("flywheel"))
        })
        .unwrap_or_else(|| {
            dirs::state_dir()
                .unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_else(|| PathBuf::from("~"))
                        .join(".local/state")
                })
                .join("flywheel")
        })
}

impl Config {
    pub fn resolve_full(
        state_dir: Option<PathBuf>,
        port: Option<u16>,
        flywheel_state: Option<PathBuf>,
        hosts: Vec<(String, String)>,
    ) -> Result<Self> {
        // bd-fpd5: XDG-first default; explicit flag wins, then env.
        let state_dir = state_dir
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(default_state_dir);
        let port = port
            .or_else(|| {
                std::env::var("DOORBELL_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(DEFAULT_PORT);
        let flywheel_state = flywheel_state.unwrap_or_else(default_flywheel_state);
        Ok(Self {
            state_dir,
            port,
            flywheel_state,
            hosts,
        })
    }

    pub fn pid_file(&self) -> PathBuf {
        self.state_dir.join("doorbell.pid")
    }

    pub fn socket_path(&self) -> PathBuf {
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(rt).join("doorbell.sock")
        } else {
            self.state_dir.join("doorbell.sock")
        }
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("doorbell.log")
    }
}

fn read_pid(path: &PathBuf) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Exclusive-create pidfile claim (symlink to the pid string). `serve`
/// claims the pidfile itself once both binds succeed — a launchd/systemd
/// started daemon is then pidfile-managed exactly like a `start` child,
/// so stop/status/restart see one management surface (the bd-start-adopt
/// lesson: only `start` used to write the pidfile, leaving a unit-started
/// daemon "foreign" forever).
#[cfg(unix)]
pub fn claim_pid_file(path: &PathBuf, pid: u32) -> std::io::Result<()> {
    let target = format!("{pid}");
    let _ = std::fs::remove_file(path); // stale claim; the bind fence is authoritative
    std::os::unix::fs::symlink(&target, path)
}
#[cfg(not(unix))]
pub fn claim_pid_file(path: &PathBuf, pid: u32) -> std::io::Result<()> {
    std::fs::write(path, format!("{pid}\n"))
}

/// Release our own pidfile claim (never drop a stranger's).
pub fn release_pid_file(path: &PathBuf, pid: u32) {
    if read_pid(path) == Some(pid) {
        let _ = std::fs::remove_file(path);
    }
}

/// Who owns the port, as far as the bind fence can tell. Pure over the
/// pidfile read + /health body so the adopt matrix stays testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerVerdict {
    /// Healthy doorbell peer, pidfile-managed (matches the pidfile pid).
    Managed(u32),
    /// Healthy doorbell peer, no (live) pidfile claim — adoptable.
    Unclaimed(u32),
    /// Port answers but not with our protocol: a squatter, refuse loudly.
    Squatter,
    /// Nothing answers /health.
    Unknown,
}

pub fn classify_owner(
    pidfile_pid: Option<u32>,
    health: Option<&serde_json::Value>,
) -> OwnerVerdict {
    let Some(h) = health else {
        return OwnerVerdict::Unknown;
    };
    if h.get("protocol").and_then(|p| p.as_str()) != Some(doorbell_rt::api::PING.protocol) {
        return OwnerVerdict::Squatter;
    }
    let owner = h
        .get("owner")
        .and_then(|o| o.get("pid"))
        .and_then(|p| p.as_u64())
        .map(|p| u32::try_from(p).unwrap_or(0));
    match (owner, pidfile_pid) {
        (Some(o), Some(p)) if o == p => OwnerVerdict::Managed(o),
        (Some(o), _) => OwnerVerdict::Unclaimed(o),
        (None, _) => OwnerVerdict::Unknown, // protocol peer without an owner row: nothing to attest
    }
}

/// Adopt a running daemon before spawning a child. A managed healthy peer
/// is adopted as-is; an unclaimed healthy peer is attested into the
/// pidfile (records its pid) so `stop`/`restart` manage it. A squatter
/// fails loudly before any child is spawned.
fn try_adopt(cfg: &Config) -> Result<Option<u32>> {
    let pf = read_pid(&cfg.pid_file());
    let pf_live = pf.filter(|p| pid_alive(*p));
    match classify_owner(pf_live, health_probe(cfg).as_ref()) {
        OwnerVerdict::Managed(p) => Ok(Some(p)),
        OwnerVerdict::Unclaimed(p) => {
            std::fs::write(cfg.pid_file(), format!("{p}\n"))
                .context("attest adopted daemon pidfile")?;
            Ok(Some(p))
        }
        OwnerVerdict::Squatter => anyhow::bail!(
            "doorbell: port {} held by a non-doorbell process, start refused",
            cfg.port
        ),
        OwnerVerdict::Unknown => Ok(None),
    }
}

fn pid_alive(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Our pidfile names a live process: managed by us.
fn managed(self_cfg: &Config) -> Option<u32> {
    read_pid(&self_cfg.pid_file()).filter(|p| pid_alive(*p))
}

fn health_get(cfg: &Config, path: &str) -> Option<String> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", cfg.port).parse().ok()?,
        Duration::from_secs(2),
    )
    .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;
    let idx = body.find("\r\n\r\n")?;
    Some(body[idx + 4..].to_string())
}

fn health_probe(cfg: &Config) -> Option<serde_json::Value> {
    let body = health_get(cfg, "/health")?;
    serde_json::from_str(&body).ok()
}
fn wait_for(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    cond()
}

/// Start: adopt an already-running daemon when one exists (managed or
/// healthy protocol peer), else spawn the detached serve child and verify
/// /health, exit 0 only when the daemon answers. Stale socket files are
/// removed after proving nothing listens; a live TCP squatter fails
/// loudly (bind fence is .3).
pub fn start(cfg: &Config) -> Result<()> {
    if let Some(pid) = try_adopt(cfg)? {
        println!(
            "doorbell: already running (pid {pid}, port {}) — adopted",
            cfg.port
        );
        return Ok(());
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.log_file())
        .with_context(|| format!("open {}", cfg.log_file().display()))?;
    let err_log = log.try_clone().context("clone log handle")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .arg("--state-dir")
        .arg(&cfg.state_dir)
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--flywheel-state-dir")
        .arg(&cfg.flywheel_state);
    for (p, w) in &cfg.hosts {
        cmd.arg("--host").arg(format!("{p}/{w}"));
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err_log)
        .spawn()
        .context("spawn serve child")?;
    // Detached by disowning: never wait on it (reparented on unix).
    let pid = child.id();
    std::mem::forget(child);
    std::fs::write(cfg.pid_file(), format!("{pid}\n")).context("write pidfile")?;
    if wait_for(|| health_probe(cfg).is_some(), START_TIMEOUT) {
        // The probe answering is not enough: a stale orphan on this port
        // answers too. The owner must be the child we just spawned — or a
        // healthy protocol peer that beat us to the bind (a unit-started
        // daemon before its first pidfile claim), which we adopt instead
        // of failing: the operator's intent (a running daemon) holds.
        let pf = read_pid(&cfg.pid_file());
        match classify_owner(pf, health_probe(cfg).as_ref()) {
            OwnerVerdict::Managed(p) if p == pid => {
                println!("doorbell: started (pid {pid}, port {})", cfg.port);
                return Ok(());
            }
            OwnerVerdict::Managed(p) | OwnerVerdict::Unclaimed(p) => {
                if pf != Some(p) {
                    let _ = std::fs::write(cfg.pid_file(), format!("{p}\n"));
                }
                println!(
                    "doorbell: already running (pid {p}, port {}) — adopted",
                    cfg.port
                );
                return Ok(());
            }
            OwnerVerdict::Squatter => anyhow::bail!(
                "doorbell: port {} answered by foreign owner, not child {pid}",
                cfg.port
            ),
            OwnerVerdict::Unknown => {}
        }
    }
    anyhow::bail!("doorbell: serve child (pid {pid}) never answered /health");
}

/// Stop: SIGTERM, 5s grace, assert port+socket dead. Refuses unmanaged
/// socket holders without --force (never clobbers a stranger's bind).
pub fn stop(cfg: &Config, force: bool) -> Result<()> {
    match managed(cfg) {
        Some(pid) => {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
            if wait_for(|| !pid_alive(pid), STOP_GRACE) {
                let _ = std::fs::remove_file(cfg.pid_file());
                println!("doorbell: stopped (pid {pid})");
                return Ok(());
            }
            anyhow::bail!("doorbell: pid {pid} ignored SIGTERM within 5s");
        }
        None => {
            if health_probe(cfg).is_some() {
                if !force {
                    anyhow::bail!(
                        "doorbell: port {} answers but no live pidfile — unmanaged holder, refusing (use --force)",
                        cfg.port
                    );
                }
                anyhow::bail!(
                    "doorbell: port {} answers but owner unknown — --force cannot kill what it cannot identify; stop the owner instead",
                    cfg.port
                );
            }
            let _ = std::fs::remove_file(cfg.pid_file());
            println!("doorbell: already stopped");
            Ok(())
        }
    }
}

pub fn restart(cfg: &Config) -> Result<()> {
    let _ = stop(cfg, false);
    start(cfg)
}

pub fn status(cfg: &Config) -> Result<()> {
    let pid = managed(cfg);
    let health = health_probe(cfg);
    // bd-enov (F17): a standby daemon with known workspaces is DEGRADED, not
    // green — it delivers no rings/digest wakes and every wake path silently
    // falls back to manual. Say so on stderr (the JSON stays intact and
    // additive: `health.degraded` + `health.standby_hint`).
    if let Some(h) = &health
        && let Some(rows) = h.get("degraded").and_then(|d| d.as_array())
        && !rows.is_empty()
    {
        let names: Vec<&str> = rows.iter().filter_map(|d| d.as_str()).collect();
        eprintln!("doorbell status: DEGRADED — {}", names.join(", "));
        if let Some(hint) = h.get("standby_hint").and_then(|v| v.as_str()) {
            eprintln!("  {hint}");
        }
    }
    let out = serde_json::json!({
        "alive": pid.is_some() && health.is_some(),
        "pid": pid,
        "port": cfg.port,
        "socket": cfg.socket_path().display().to_string(),
        "health": health,
    });
    println!("{out}");
    Ok(())
}

/// Install the autostart unit (idempotent) and load it: macOS launchd
/// agent, Linux systemd user unit. Template placeholders take the live
/// binary path + state dir; `serve` reads both back from argv/env so the
/// unit and the pidfile path never diverge. Other platforms: loud error
/// naming manual `doorbell start`.
pub fn install(cfg: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let body = render_autostart(
        &exe.display().to_string(),
        &cfg.state_dir.display().to_string(),
        cfg.port,
        &cfg.flywheel_state.display().to_string(),
        &cfg.hosts,
    );
    install_rendered(cfg, &body)
}

/// Render the platform autostart unit with live paths (pure — golden
/// tested). The unit reproduces the `start` serve argv (`--state-dir`,
/// `--port`, `--flywheel-state-dir`, repeatable `--host p/w`) so an
/// autostarted daemon hosts the same live workspaces as a `start` child
/// and the unit and the pidfile path never diverge (bd-0zcr.2 unit 4).
/// Empty hosts = standby (bind + reads only), same as `start`.
pub fn render_autostart(
    exe: &str,
    state_dir: &str,
    port: u16,
    flywheel_state: &str,
    hosts: &[(String, String)],
) -> String {
    #[cfg(target_os = "macos")]
    {
        let mut args = vec![
            "<string>--state-dir</string>".to_string(),
            format!("<string>{state_dir}</string>"),
            "<string>--port</string>".to_string(),
            format!("<string>{port}</string>"),
            "<string>--flywheel-state-dir</string>".to_string(),
            format!("<string>{flywheel_state}</string>"),
        ];
        for (p, w) in hosts {
            args.push("<string>--host</string>".to_string());
            args.push(format!("<string>{p}/{w}</string>"));
        }
        include_str!("../units/com.doorbell.serve.plist")
            .replace("DOORBELL_BIN", exe)
            .replace("DOORBELL_STATE_DIR_VALUE", state_dir)
            .replace("DOORBELL_STATE_DIR_KEY", "DOORBELL_STATE_DIR")
            .replace("DOORBELL_SERVE_ARGS", &args.join("\n        "))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut args = vec![
            "--state-dir".to_string(),
            state_dir.to_string(),
            "--port".to_string(),
            port.to_string(),
            "--flywheel-state-dir".to_string(),
            flywheel_state.to_string(),
        ];
        for (p, w) in hosts {
            args.push("--host".to_string());
            args.push(format!("{p}/{w}"));
        }
        include_str!("../units/doorbell.service")
            .replace("DOORBELL_BIN", exe)
            .replace("DOORBELL_STATE_DIR_VALUE", state_dir)
            .replace("DOORBELL_STATE_DIR_KEY", "DOORBELL_STATE_DIR")
            .replace("DOORBELL_SERVE_ARGS", &args.join(" "))
    }
}

fn install_rendered(_cfg: &Config, body: &str) -> Result<()> {
    {
        let home = std::env::var("HOME").context("HOME unset")?;
        let dir = PathBuf::from(home).join("Library").join("LaunchAgents");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("com.doorbell.serve.plist");
        std::fs::write(&path, body)?;
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "501".to_string());
        let domain = format!("gui/{uid}/com.doorbell.serve");
        let boot = std::process::Command::new("launchctl")
            .args([
                "bootstrap",
                &format!("gui/{uid}"),
                &path.display().to_string(),
            ])
            .output();
        match boot {
            Ok(o) if o.status.success() => {}
            _ => {
                // Already loaded (or kicked): kickstart -k restarts into the
                // current binary instead of failing install.
                let kick = std::process::Command::new("launchctl")
                    .args(["kickstart", "-k", &domain])
                    .output();
                if !kick.map(|o| o.status.success()).unwrap_or(false) {
                    anyhow::bail!("launchctl bootstrap + kickstart both failed for {domain}");
                }
            }
        }
        println!("doorbell: autostart installed ({})", path.display());
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })
            .context("neither XDG_CONFIG_HOME nor HOME set")?;
        let dir = base.join("systemd").join("user");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("doorbell.service");
        std::fs::write(&path, body)?;
        for args in [
            vec!["--user", "daemon-reload"],
            vec!["--user", "enable", "doorbell.service"],
        ] {
            let st = std::process::Command::new("systemctl")
                .args(&args)
                .status()
                .context("systemctl failed (no user systemd?)")?;
            if !st.success() {
                anyhow::bail!("systemctl {} failed", args.join(" "));
            }
        }
        println!("doorbell: autostart installed ({})", path.display());
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        anyhow::bail!(
            "no autostart unit for this platform; run `doorbell start` (pidfile) instead"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_ladder_prefers_explicit_env_then_xdg_then_home() {
        // bd-fpd5: DOORBELL_STATE_DIR > XDG_STATE_HOME/doorbell >
        // ~/.local/state/doorbell. Pure ladder (no env reads) so tests
        // stay hermetic under parallel execution.
        let home = PathBuf::from("/home/u");
        assert_eq!(
            resolve_state_dir_default(
                Some(PathBuf::from("/env/doorbell")),
                Some(PathBuf::from("/xdg")),
                Some(home.clone())
            ),
            PathBuf::from("/env/doorbell")
        );
        assert_eq!(
            resolve_state_dir_default(None, Some(PathBuf::from("/xdg")), Some(home.clone())),
            PathBuf::from("/xdg/doorbell")
        );
        assert_eq!(
            resolve_state_dir_default(None, None, Some(home)),
            PathBuf::from("/home/u/.local/state/doorbell")
        );
    }

    #[test]
    fn default_state_ladder_ignores_empty_env() {
        // Empty DOORBELL_STATE_DIR / XDG_STATE_HOME are unset, never "".
        assert_eq!(
            resolve_state_dir_default(
                Some(PathBuf::from("")),
                Some(PathBuf::from("")),
                Some(PathBuf::from("/home/u"))
            ),
            PathBuf::from("/home/u/.local/state/doorbell")
        );
    }

    #[test]
    fn legacy_platform_dir_is_data_dir_doorbell() {
        // The pre-XDG default we migrate away from, when it exists.
        assert_eq!(
            legacy_platform_state_dir(),
            dirs::data_dir().map(|d| d.join("doorbell"))
        );
    }

    fn tmp_lifecycle(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbcl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn migrate_once_copies_when_new_missing() {
        let base = tmp_lifecycle("mig-new");
        let old = base.join("old");
        std::fs::create_dir_all(old.join("journals")).unwrap();
        std::fs::write(old.join("doorbell-wake-w.json"), r#"{"w":1}"#).unwrap();
        std::fs::write(
            old.join("journals").join("doorbell-journal-w.jsonl"),
            "{\"v\":1}\n",
        )
        .unwrap();
        let new = base.join("new");
        let copied = maybe_migrate_to_default(&new, &old)
            .unwrap()
            .expect("missing new dir must migrate");
        assert_eq!(copied.len(), 2);
        assert_eq!(
            std::fs::read(new.join("doorbell-wake-w.json")).unwrap(),
            std::fs::read(old.join("doorbell-wake-w.json")).unwrap()
        );
        // Second run is a no-op (migrate-once): new is populated now.
        assert!(
            maybe_migrate_to_default(&new, &old).unwrap().is_none(),
            "populated new dir must not re-migrate"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_once_skips_when_new_populated() {
        // Refuse-overwrite is never even attempted: populated new wins.
        let base = tmp_lifecycle("mig-skip");
        let old = base.join("old");
        let new = base.join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("doorbell-wake-w.json"), r#"{"new":1}"#).unwrap();
        std::fs::write(old.join("doorbell-wake-w.json"), r#"{"old":1}"#).unwrap();
        assert!(
            maybe_migrate_to_default(&new, &old).unwrap().is_none(),
            "populated new dir must be left alone"
        );
        assert_eq!(
            std::fs::read(new.join("doorbell-wake-w.json")).unwrap(),
            br#"{"new":1}"#.to_vec()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_once_skips_when_old_missing() {
        let base = tmp_lifecycle("mig-noold");
        let new = base.join("new");
        assert!(
            maybe_migrate_to_default(&new, &base.join("nope"))
                .unwrap()
                .is_none(),
            "absent old dir is a clean no-op"
        );
        assert!(!new.exists(), "no-op must not create the new dir");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_host_accepts_absolute_project_paths() {
        // The scratch/harness shape: absolute project, bare workspace name.
        assert_eq!(
            parse_host("/tmp/scratch/proj/flywheel-proj").unwrap(),
            ("/tmp/scratch/proj".to_string(), "flywheel-proj".to_string()),
            "absolute project paths must be hostable"
        );
        // Relative slug stays as before.
        assert_eq!(
            parse_host("dbell/flywheel-dbell").unwrap(),
            ("dbell".to_string(), "flywheel-dbell".to_string())
        );
        // Root-anchored single segment is a legal project key.
        assert_eq!(
            parse_host("/proj/ws").unwrap(),
            ("/proj".to_string(), "ws".to_string())
        );
    }

    #[test]
    fn parse_host_refuses_shapes_it_cannot_address() {
        for bad in ["", "onlyslug", "/ws", "proj/", "/"] {
            assert!(
                parse_host(bad).is_err(),
                "{bad:?} must be refused (project AND workspace are required)"
            );
        }
    }

    #[test]
    fn render_substitutes_live_paths() {
        let hosts = vec![("proj".to_string(), "ws".to_string())];
        let body = render_autostart("/opt/doorbell", "/var/dbll", 18980, "/var/fw", &hosts);
        assert!(body.contains("/opt/doorbell"), "binary path rendered");
        assert!(body.contains("/var/dbll"), "state dir rendered");
        assert!(body.contains("18980"), "port rendered");
        assert!(body.contains("/var/fw"), "flywheel state dir rendered");
        assert!(body.contains("proj/ws"), "--host pair rendered");
        assert!(!body.contains("DOORBELL_BIN"), "no placeholder survives");
        // bd-y6gw: the env key resolves to the literal `DOORBELL_STATE_DIR`
        // (a real env name), so absence is asserted on the _KEY/_VALUE
        // placeholder tokens, not the resolved name.
        assert!(
            !body.contains("DOORBELL_STATE_DIR_VALUE"),
            "value placeholder resolved"
        );
        assert!(
            !body.contains("DOORBELL_STATE_DIR_KEY"),
            "key placeholder resolved"
        );
        assert!(
            body.contains("DOORBELL_STATE_DIR"),
            "env key resolves to the literal DOORBELL_STATE_DIR name"
        );
        assert!(
            !body.contains("DOORBELL_SERVE_ARGS"),
            "no placeholder survives"
        );
    }

    #[test]
    fn render_empty_hosts_is_standby() {
        // No --host = standby (bind + reads only), same as `start`.
        let body = render_autostart("/opt/doorbell", "/var/dbll", 18980, "/var/fw", &[]);
        assert!(!body.contains("--host"), "standby carries no --host");
        assert!(
            !body.contains("DOORBELL_SERVE_ARGS"),
            "no placeholder survives"
        );
    }

    fn health(pid: u64) -> serde_json::Value {
        serde_json::json!({
            "protocol": doorbell_rt::api::PING.protocol,
            "owner": {"pid": pid},
        })
    }

    #[test]
    fn classify_owner_matrix() {
        // Managed: health owner matches the live pidfile pid.
        assert_eq!(
            classify_owner(Some(42), Some(&health(42))),
            OwnerVerdict::Managed(42)
        );
        // Unclaimed: healthy peer but no live pidfile claim (launchd daemon
        // before its first claim, or a dead-pid pidfile).
        assert_eq!(
            classify_owner(None, Some(&health(42))),
            OwnerVerdict::Unclaimed(42)
        );
        assert_eq!(
            classify_owner(Some(999_999_999), Some(&health(42))),
            OwnerVerdict::Unclaimed(42)
        );
        // Squatter: port answers without our protocol.
        let squatter = serde_json::json!({"hello": "world"});
        assert_eq!(
            classify_owner(None, Some(&squatter)),
            OwnerVerdict::Squatter
        );
        // Unknown: nothing answers.
        assert_eq!(classify_owner(Some(42), None), OwnerVerdict::Unknown);
        // Protocol peer without an owner row: nothing to attest.
        let no_owner = serde_json::json!({"protocol": doorbell_rt::api::PING.protocol});
        assert_eq!(classify_owner(None, Some(&no_owner)), OwnerVerdict::Unknown);
    }
}

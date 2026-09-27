// governed-by: ADR-0012
//! Daemon lifecycle (D2): paths, pidfile, spawn, verify, stop, status.
//! Units ship `Restart=on-failure` (a clean exit over a port conflict must
//! STAY dead — the bd-serve-double-spawn lesson); no `always`.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 18980;
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a SIGTERM'd daemon gets to drain before `stop` escalates to
/// SIGKILL. Must stay ABOVE serve's DRAIN_BUDGET (10s): the old 5s beat
/// every realistic drain, so healthy shutdowns were misreported as
/// "pid ignored SIGTERM" and restart aborted (bd-xwgh round 2).
/// bd-pf3b raised 15s → 20s: the grace must also cover one probe window
/// (200ms) + EXIT_PATH_ALLOWANCE — during `mbx install` the stragglers rode
/// the full drain and the post-budget tail blew the old 15s → SIGKILL →
/// respawn race → "port still held".
const STOP_GRACE: Duration = Duration::from_secs(20);

/// bd-pf3b: what STOP_GRACE must cover AFTER the drain budget expires —
/// the straggler note, the liveness probe window, and finish() (leases,
/// pidfile, process exit) under load. Sized from the live incident: during
/// `mbx install` the post-budget tail alone pushed the total past the old
/// 15s grace → SIGKILL → respawn race → "port still held".
/// Test-only: STOP_GRACE (the production number) is asserted against it.
#[cfg(test)]
const EXIT_PATH_ALLOWANCE: Duration = Duration::from_secs(5);
/// Extra patience after our own SIGKILL before declaring the pid
/// unkillable (stuck in uninterruptible state).
const KILL_GRACE: Duration = Duration::from_secs(5);

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

/// Exclusive-create pidfile claim. `serve` claims the regular pidfile itself
/// once both binds succeed — a launchd/systemd started daemon is then
/// pidfile-managed exactly like a `start` child, so stop/status/restart see
/// one management surface (the bd-start-adopt lesson: only `start` used to
/// write the pidfile, leaving a unit-started daemon "foreign" forever).
#[cfg(unix)]
pub fn claim_pid_file(path: &PathBuf, pid: u32) -> std::io::Result<()> {
    // Keep the pidfile as a regular file. A relative symlink whose target is
    // the PID string is dangling, so status/restart lose ownership of a
    // healthy daemon immediately after serve claims it.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{pid}\n"))
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

/// What one tick of the `start` watch observed: a dead child first (it
/// must never be masked by a peer answering /health on this port), then
/// health, then the deadline. Pure — tests script all three observations
/// with no clock, child, or socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boot {
    Healthy,
    Exited(std::process::ExitStatus),
    TimedOut,
}

fn boot_probe(
    exited: Option<std::process::ExitStatus>,
    health_answered: bool,
    timed_out: bool,
) -> Option<Boot> {
    if let Some(status) = exited {
        return Some(Boot::Exited(status));
    }
    if health_answered {
        return Some(Boot::Healthy);
    }
    timed_out.then_some(Boot::TimedOut)
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

/// The daemon's own pid as it reports it in `/health.owner.pid`.
fn owner_pid(health: &serde_json::Value) -> Option<u32> {
    health
        .get("owner")
        .and_then(|o| o.get("pid"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
}

/// The status envelope's liveness view, as a pure fold so it is testable
/// without a server.
///
/// bd-doorbell-status-alive-h9hb. `managed()` answers a real question — did WE
/// start this, and is the pid we hold still running — but that is attribution,
/// and it was being used as proof of service. The pid file is read from the
/// *caller's* `--state-dir`, so a caller configured with a different one than
/// the daemon's reports a null pid for a daemon that is up and answering. That
/// is not hypothetical: `flywheel doorbell status` passes the FLYWHEEL state
/// dir, and so printed `alive:false, pid:null` beside a full health body.
///
/// `alive` therefore comes from the probe that actually reached the daemon.
/// `pid` still prefers the local pid file, because that is the pid we can
/// signal, and falls back to the daemon's own report; `pid_source` names which
/// one won so the two can never be silently confused.
fn status_liveness(
    local_pid: Option<u32>,
    health: Option<&serde_json::Value>,
) -> (bool, Option<u32>, Option<&'static str>) {
    let alive = health.is_some();
    if let Some(pid) = local_pid {
        return (alive, Some(pid), Some("pid_file"));
    }
    match health.and_then(owner_pid) {
        Some(pid) => (alive, Some(pid), Some("health")),
        None => (alive, None, None),
    }
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

fn tcp_endpoint_alive(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}")
            .parse()
            .expect("loopback address"),
        Duration::from_millis(100),
    )
    .is_ok()
}

#[cfg(unix)]
fn socket_endpoint_alive(path: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
fn socket_endpoint_alive(_path: &std::path::Path) -> bool {
    false
}

/// Reap a serve child we spawned but cannot use (bind-race loser, died
/// before /health, health timeout). Only OUR pid is waited on: the TCP
/// endpoint may legitimately be held by the winner of the bind race, and
/// waiting for that burned a full grace on every adoption.
fn reap_failed_start(cfg: &Config, pid: u32) {
    if pid_alive(pid) {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
        if !wait_for(|| !pid_alive(pid), STOP_GRACE) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            let _ = wait_for(|| !pid_alive(pid), KILL_GRACE);
        }
    }
    if read_pid(&cfg.pid_file()) == Some(pid) {
        let _ = std::fs::remove_file(cfg.pid_file());
    }
    if !socket_endpoint_alive(&cfg.socket_path()) {
        let _ = std::fs::remove_file(cfg.socket_path());
    }
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

/// Unload the supervisor, reporting a REAL failure (bd-yvte): the old
/// `let _ = .output()` swallowed it, SIGKILL then respawned through
/// KeepAlive, and the endpoint bail blamed "another process" for our own
/// job. Absence counts as success — bootout of a never-loaded job (manual
/// start) must never block a stop.
fn disable_supervisor(cfg: &Config) -> Result<(), String> {
    // Never unload THE service on behalf of a scratch instance (tests and
    // probes run `stop` against temp state dirs while the real unit is
    // loaded — an unconditional bootout there killed the live daemon).
    if let Some(body) = installed_unit_path().and_then(|p| std::fs::read_to_string(p).ok())
        && !unit_body_serves(&body, &cfg.state_dir)
    {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(uid) = uid {
            let out = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/com.doorbell.serve")])
                .output()
                .map_err(|e| format!("launchctl bootout could not run: {e}"))?;
            if !out.status.success() {
                let text = format!(
                    "{} {}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                if !bootout_absent(&text) {
                    return Err(format!(
                        "launchctl bootout failed ({}): {}",
                        out.status,
                        text.trim()
                    ));
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "stop", "doorbell.service"])
            .output()
            .map_err(|e| format!("systemctl stop could not run: {e}"))?;
        if !out.status.success() {
            let text = format!(
                "{} {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if !bootout_absent(&text) {
                return Err(format!(
                    "systemctl stop failed ({}): {}",
                    out.status,
                    text.trim()
                ));
            }
        }
    }
    Ok(())
}

/// Absence signatures of `bootout`/`stop` on a job that is not loaded —
/// the goal (nothing left to respawn) is already met, so those count as
/// success. Pure, so the classification is unit-tested without launchd.
fn bootout_absent(output: &str) -> bool {
    let s = output.to_ascii_lowercase();
    ["no such process", "could not find service", "not loaded"]
        .iter()
        .any(|p| s.contains(p))
}

/// The installed unit serves this state dir (its argv names it). A unit
/// we cannot READ is assumed to be THE service: a loaded job whose plist
/// vanished still needs stopping, and silence there is how a respawn war
/// starts.
fn unit_body_serves(body: &str, state_dir: &std::path::Path) -> bool {
    body.contains(&state_dir.display().to_string())
}

/// Where the installed autostart unit lives on this platform. Must stay in
/// lockstep with `install_rendered` (same file paths) — a drift would turn
/// every supervised start into a silent detached fallback.
fn installed_unit_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("LaunchAgents")
                .join("com.doorbell.serve.plist"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(base.join("systemd").join("user").join("doorbell.service"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// The pid a live /health attests as its owner (the protocol cannot lie
/// about its own pid — this covers a lost pidfile).
fn health_owner_pid(cfg: &Config) -> Option<u32> {
    health_probe(cfg)?
        .get("owner")
        .and_then(|o| o.get("pid"))
        .and_then(|p| p.as_u64())
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p > 1)
        .filter(|p| pid_alive(*p))
}

/// Who `stop` signals: the protocol-attested owner when a doorbell is
/// answering, else a live pidfile pid. None = nothing of ours to stop.
fn stop_target(cfg: &Config) -> Option<u32> {
    health_owner_pid(cfg).or_else(|| read_pid(&cfg.pid_file()).filter(|p| pid_alive(*p)))
}

/// Start through the installed supervisor (launchd/systemd). `Ok(Some)`
/// = healthy and attested, `Ok(None)` = the supervisor could not be asked
/// (caller falls back to a detached child), `Err` = asked, but it never
/// became healthy — truthfully, with the log tail.
fn start_via_supervisor(cfg: &Config, unit: &std::path::Path) -> Result<Option<u32>> {
    let asked = {
        #[cfg(target_os = "macos")]
        {
            let uid = std::process::Command::new("id")
                .arg("-u")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            match uid {
                None => false,
                Some(uid) => {
                    // Reload semantics (bd-jvu7): a stale loaded definition
                    // must be dropped, not kickstarted — kickstart restarts
                    // the OLD def and file edits never reach the job.
                    match run_reload_plan(&reload_plan(&uid, unit)) {
                        Ok(()) => true,
                        Err(e) => {
                            eprintln!("doorbell: {e}");
                            false
                        }
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            let _ = unit;
            std::process::Command::new("systemctl")
                .args(["--user", "start", "doorbell.service"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = unit;
            false
        }
    };
    if !asked {
        return Ok(None);
    }
    if wait_for(|| health_probe(cfg).is_some(), START_TIMEOUT) {
        if let Some(pid) = health_owner_pid(cfg) {
            // serve claims the pidfile itself after bind; this attests it
            // even if the claim raced a previous owner.
            if read_pid(&cfg.pid_file()) != Some(pid) {
                let _ = std::fs::write(cfg.pid_file(), format!("{pid}\n"));
            }
            return Ok(Some(pid));
        }
        anyhow::bail!(
            "{}",
            start_failure_brief(
                cfg.port,
                "the supervised daemon",
                "answered /health without an attestable owner pid",
                tcp_endpoint_alive(cfg.port),
                &cfg.log_file(),
            )
        );
    }
    anyhow::bail!(
        "{}",
        start_failure_brief(
            cfg.port,
            "the supervised daemon",
            &format!("never answered /health within {START_TIMEOUT:?}"),
            tcp_endpoint_alive(cfg.port),
            &cfg.log_file(),
        )
    )
}

/// Last `n` non-empty lines of a log, read from the tail only (the daemon
/// log grows unbounded; reading it whole to print five lines would not be
/// proportional to the work).
fn tail_lines(path: &std::path::Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(4096)))
        .is_err()
    {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    // Filter is not ExactSize, so no rev().take(n).rev() chain: collect
    // once and slice the end.
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| (*s).to_string()).collect()
}

/// The one place a failed `start` explains itself: who failed, why, what
/// the port looked like, and the tail of the daemon log — the real cause
/// ("port held by non-doorbell squatter", "live lease", …) lives there,
/// not in the generic timeout the old code reported.
fn start_failure_brief(
    port: u16,
    who: &str,
    cause: &str,
    port_answers: bool,
    log: &std::path::Path,
) -> String {
    let mut out = format!("doorbell: {who} {cause}\n");
    out.push_str(&format!(
        "  port {port}: {}\n",
        if port_answers {
            "ANSWERING now — another process owns it"
        } else {
            "not answering"
        }
    ));
    let tail = tail_lines(log, 5);
    if tail.is_empty() {
        out.push_str(&format!("  no log output in {}", log.display()));
    } else {
        out.push_str(&format!("  last lines from {}:\n", log.display()));
        for line in tail {
            out.push_str(&format!("    | {line}\n"));
        }
    }
    out.trim_end().to_string()
}

/// Start: adopt an already-running daemon when one exists (managed or
/// healthy protocol peer), wait out a busy-but-unresponsive port, prefer
/// the byte-identical installed supervisor unit (so `restart` ends
/// supervised, not as a detached child), else spawn the detached serve
/// child and watch BOTH its /health and its exit — a child that dies at
/// the bind fence fails immediately with the real cause from the log.
pub fn start(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("create state dir {}", cfg.state_dir.display()))?;
    if let Some(pid) = try_adopt(cfg)? {
        println!(
            "doorbell: already running (pid {pid}, port {}) — adopted",
            cfg.port
        );
        return Ok(());
    }
    let exe = std::env::current_exe().context("current_exe")?;
    // A busy port with no healthy peer is usually the previous daemon
    // still draining. The check costs nothing when the port is clear
    // (connect fails instantly); spawning into a busy port only produces a
    // child that dies at the bind fence — the old "never answered
    // /health" trap whose real cause was buried in the log.
    if !wait_for(
        || !tcp_endpoint_alive(cfg.port) || health_probe(cfg).is_some(),
        START_TIMEOUT,
    ) {
        return match try_adopt(cfg)? {
            Some(pid) => {
                println!(
                    "doorbell: already running (pid {pid}, port {}) — adopted",
                    cfg.port
                );
                Ok(())
            }
            // A Squatter verdict propagates through the `?` above.
            None => anyhow::bail!(
                "{}",
                start_failure_brief(
                    cfg.port,
                    "the port holder",
                    &format!(
                        "never answers /health — stop it (lsof -nP -iTCP:{}) and retry",
                        cfg.port
                    ),
                    true,
                    &cfg.log_file(),
                )
            ),
        };
    }
    if let Some(pid) = try_adopt(cfg)? {
        println!(
            "doorbell: already running (pid {pid}, port {}) — adopted",
            cfg.port
        );
        return Ok(());
    }
    // Supervised start, but only when the installed unit renders
    // byte-identically for THIS config (same binary/state/port/hosts).
    // `stop` unloads the unit, so without this a `restart` silently ends
    // as a detached child: no crash respawn, no autostart after reboot.
    // A differing unit (test binary, temp state dir) is left untouched.
    if let Some(unit) = installed_unit_path()
        && let Ok(existing) = std::fs::read_to_string(&unit)
    {
        let rendered = render_autostart(
            &exe.display().to_string(),
            &cfg.state_dir.display().to_string(),
            cfg.port,
            &cfg.flywheel_state.display().to_string(),
            &cfg.hosts,
            &toron_mcp_url(),
        );
        if existing == rendered {
            match start_via_supervisor(cfg, &unit)? {
                Some(pid) => {
                    println!(
                        "doorbell: started (pid {pid}, port {}, supervised)",
                        cfg.port
                    );
                    return Ok(());
                }
                None => eprintln!(
                    "doorbell: supervisor unusable from this session — falling back to a detached child"
                ),
            }
        } else {
            eprintln!(
                "doorbell: installed unit does not match this config (other binary or state dir) — starting detached"
            );
        }
    }
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
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err_log)
        .spawn()
        .context("spawn serve child")?;
    // Claim the pid before detaching. If the claim fails, terminate the
    // child instead of returning an error with an untracked daemon behind it.
    let pid = child.id();
    if let Err(error) = std::fs::write(cfg.pid_file(), format!("{pid}\n")) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).with_context(|| format!("write pidfile {}", cfg.pid_file().display()));
    }
    // Watch the child ITSELF while waiting for /health: serve dies fast
    // and loudly on a conflict (bind fence, live lease) and its real
    // reason is in the log tail — waiting out START_TIMEOUT for a corpse
    // only ever produced the misleading "never answered /health".
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    let boot = loop {
        let exited = child.try_wait().context("poll serve child")?;
        // No health probe once the child is gone: a dead child must never
        // be masked by a peer answering on this port.
        let health_answered = exited.is_none() && health_probe(cfg).is_some();
        let timed_out = std::time::Instant::now() >= deadline;
        if let Some(decision) = boot_probe(exited, health_answered, timed_out) {
            break decision;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    match boot {
        Boot::Healthy => {
            // The probe answering is not enough: a stale orphan on this port
            // answers too. The owner must be the child we just spawned — or a
            // healthy protocol peer that beat us to the bind (a unit-started
            // daemon before its first pidfile claim), which we adopt instead
            // of failing: the operator's intent (a running daemon) holds.
            let pf = read_pid(&cfg.pid_file());
            match classify_owner(pf, health_probe(cfg).as_ref()) {
                OwnerVerdict::Managed(p) if p == pid => {
                    // Detached by disowning: never wait on it (reparented on unix).
                    std::mem::forget(child);
                    println!("doorbell: started (pid {pid}, port {})", cfg.port);
                    Ok(())
                }
                OwnerVerdict::Managed(p) | OwnerVerdict::Unclaimed(p) => {
                    if p != pid {
                        // Another daemon won the bind race; do not leave our
                        // losing child alive behind the adopted peer.
                        reap_failed_start(cfg, pid);
                    }
                    if pf != Some(p) {
                        let _ = std::fs::write(cfg.pid_file(), format!("{p}\n"));
                    }
                    std::mem::forget(child);
                    println!(
                        "doorbell: already running (pid {p}, port {}) — adopted",
                        cfg.port
                    );
                    Ok(())
                }
                OwnerVerdict::Squatter => {
                    reap_failed_start(cfg, pid);
                    anyhow::bail!(
                        "doorbell: port {} answered by foreign owner, not child {pid}",
                        cfg.port
                    )
                }
                OwnerVerdict::Unknown => {
                    reap_failed_start(cfg, pid);
                    anyhow::bail!(
                        "{}",
                        start_failure_brief(
                            cfg.port,
                            &format!("serve child (pid {pid})"),
                            "answered /health without an attestable owner pid",
                            true,
                            &cfg.log_file(),
                        )
                    )
                }
            }
        }
        Boot::Exited(status) => {
            // Exit 0 usually means a healthy peer won the race — adopt it
            // instead of failing the operator's intent.
            if let Some(peer) = try_adopt(cfg)? {
                println!(
                    "doorbell: already running (pid {peer}, port {}) — adopted",
                    cfg.port
                );
                return Ok(());
            }
            let port_answers = tcp_endpoint_alive(cfg.port);
            reap_failed_start(cfg, pid);
            anyhow::bail!(
                "{}",
                start_failure_brief(
                    cfg.port,
                    &format!("serve child (pid {pid})"),
                    &format!("exited before /health answered ({status})"),
                    port_answers,
                    &cfg.log_file(),
                )
            )
        }
        Boot::TimedOut => {
            let port_answers = tcp_endpoint_alive(cfg.port);
            reap_failed_start(cfg, pid);
            anyhow::bail!(
                "{}",
                start_failure_brief(
                    cfg.port,
                    &format!("serve child (pid {pid})"),
                    &format!("never answered /health within {START_TIMEOUT:?}"),
                    port_answers,
                    &cfg.log_file(),
                )
            )
        }
    }
}

/// Stop: SIGTERM with a drain-sized grace (SIGKILL escalation), then
/// assert the endpoints are dead. Refuses unmanaged
/// socket holders without --force (never clobbers a stranger's bind).
pub fn stop(cfg: &Config, force: bool) -> Result<()> {
    let msg = stop_with_budgets(cfg, force, STOP_GRACE, KILL_GRACE)?;
    println!("{msg}");
    Ok(())
}

/// Budget-injected wrapper: the policy numbers are data, and the world
/// behind them is one seam (see [`StopWorld`]).
fn stop_with_budgets(
    cfg: &Config,
    force: bool,
    grace: Duration,
    kill_grace: Duration,
) -> Result<String> {
    stop_core(
        &mut RealStop { cfg },
        cfg.port,
        &cfg.socket_path(),
        force,
        grace,
        kill_grace,
    )
}

/// The signal stop delivers: `Term` first, `Kill` only as escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sig {
    Term,
    Kill,
}

/// The world `stop` touches — one seam so the interpreter test scripts the
/// process, endpoints, and supervisor as plain data (effect law: clock and
/// services substituted; no real sleep, signal, or socket in the decision
/// test).
trait StopWorld {
    /// First pid to signal: live /health owner, else live pidfile pid.
    fn stop_target(&mut self) -> Option<u32>;
    fn signal(&mut self, pid: u32, sig: Sig);
    /// Poll until the pid is dead or `grace` elapses; true = dead.
    fn wait_dead(&mut self, pid: u32, grace: Duration) -> bool;
    fn unload_supervisor(&mut self) -> Result<(), String>;
    fn health_answers(&mut self) -> bool;
    fn tcp_alive(&mut self, port: u16) -> bool;
    fn uds_alive(&mut self, path: &std::path::Path) -> bool;
    /// Managed branch: cleanup after the pid is proven dead.
    fn cleanup_state(&mut self, pid: u32);
    /// Nothing-of-ours branch: drop the stale claim and stale socket path.
    fn cleanup_stale(&mut self, path: &std::path::Path);
}

/// Real world behind [`StopWorld`], bound to this config.
struct RealStop<'a> {
    cfg: &'a Config,
}

impl StopWorld for RealStop<'_> {
    fn stop_target(&mut self) -> Option<u32> {
        stop_target(self.cfg)
    }
    fn signal(&mut self, pid: u32, sig: Sig) {
        let mut cmd = std::process::Command::new("kill");
        match sig {
            Sig::Term => {
                cmd.arg(pid.to_string());
            }
            Sig::Kill => {
                cmd.args(["-KILL", &pid.to_string()]);
            }
        }
        let _ = cmd.status();
    }
    fn wait_dead(&mut self, pid: u32, grace: Duration) -> bool {
        wait_for(|| !pid_alive(pid), grace)
    }
    fn unload_supervisor(&mut self) -> Result<(), String> {
        disable_supervisor(self.cfg)
    }
    fn health_answers(&mut self) -> bool {
        health_probe(self.cfg).is_some()
    }
    fn tcp_alive(&mut self, port: u16) -> bool {
        tcp_endpoint_alive(port)
    }
    fn uds_alive(&mut self, path: &std::path::Path) -> bool {
        socket_endpoint_alive(path)
    }
    fn cleanup_state(&mut self, pid: u32) {
        if read_pid(&self.cfg.pid_file()) == Some(pid) {
            let _ = std::fs::remove_file(self.cfg.pid_file());
        }
        // The caller proved the endpoint dead: what remains of the socket
        // path is stale by definition.
        let _ = std::fs::remove_file(self.cfg.socket_path());
    }
    fn cleanup_stale(&mut self, path: &std::path::Path) {
        let _ = std::fs::remove_file(self.cfg.pid_file());
        if !socket_endpoint_alive(path) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The stop decision itself: observations and policy only, no I/O — tests
/// script a clean exit, a hanging drain, an unkillable pid, a foreign
/// endpoint, and the unmanaged/already-stopped arms through [`StopWorld`].
fn stop_core<W: StopWorld>(
    world: &mut W,
    port: u16,
    socket: &std::path::Path,
    force: bool,
    grace: Duration,
    kill_grace: Duration,
) -> Result<String> {
    let Some(pid) = world.stop_target() else {
        if world.health_answers() || world.tcp_alive(port) {
            if !force {
                anyhow::bail!(
                    "doorbell: port {port} answers but no doorbell owner or live pidfile — unmanaged holder, refusing (use --force)"
                );
            }
            anyhow::bail!(
                "doorbell: port {port} answers but owner unknown — --force cannot kill what it cannot identify; stop the owner instead (lsof -nP -iTCP:{port})"
            );
        }
        world.cleanup_stale(socket);
        return Ok("doorbell: already stopped".to_string());
    };
    // TERM first, unload second: serve needs its full drain budget, and
    // launchd's bootout SIGKILLs at its 5s exit timeout — booting out
    // first beheaded the drain and made healthy daemons look like
    // SIGTERM-ignorers. bd-pf3b: on escalation the unload moves BEFORE
    // the SIGKILL (still after the full grace): KeepAlive respawns an
    // unclean exit sub-second, so kill-then-unload raced a respawn that
    // grabbed the port ("stopped but still held"). The job is dropped
    // first, so the unclean exit has nothing to respawn it.
    world.signal(pid, Sig::Term);
    let mut escalated = false;
    if !world.wait_dead(pid, grace) {
        eprintln!("doorbell: pid {pid} still alive after {grace:?} grace — sending SIGKILL");
        // bd-yvte: the unload IS the respawn guard (bd-pf3b), so its failure
        // can never be swallowed: retry once (launchd busy), then refuse the
        // kill — SIGKILL into a loaded job respawns sub-second and the
        // endpoint bail would misattribute our own respawn as a squatter.
        if let Err(first) = world.unload_supervisor() {
            eprintln!("doorbell: supervisor unload failed ({first}) — retrying once");
            if let Err(second) = world.unload_supervisor() {
                anyhow::bail!(
                    "doorbell: cannot stop pid {pid} — supervisor unload failed twice ({second}; first: {first}); SIGKILL would be respawned by the supervisor. Unload it manually (launchctl bootout / systemctl stop doorbell.service) and retry"
                );
            }
        }
        world.signal(pid, Sig::Kill);
        escalated = true;
        if !world.wait_dead(pid, kill_grace) {
            anyhow::bail!(
                "doorbell: pid {pid} survived SIGKILL — investigate (lsof -nP -iTCP:{port}) before retrying"
            );
        }
    } else {
        // Clean exit: a 0-exit is not respawned (KeepAlive guards on
        // unsuccessful exit), but a still-loaded definition goes stale on
        // the next edit — loud, not fatal (bd-yvte: never silent).
        if let Err(e) = world.unload_supervisor() {
            eprintln!(
                "doorbell: supervisor unload failed after clean exit ({e}) — run `doorbell install` to reload the definition"
            );
        }
    }
    // The pid is gone; a live endpoint now belongs to a DIFFERENT process.
    // Never report success for a port we no longer own.
    if world.tcp_alive(port) || world.uds_alive(socket) {
        anyhow::bail!(
            "doorbell: pid {pid} stopped but port {port} (or its socket) is still held by another process — identify it with lsof -nP -iTCP:{port} and stop it"
        );
    }
    world.cleanup_state(pid);
    Ok(if escalated {
        format!(
            "doorbell: stopped (pid {pid}, SIGKILL after {}s grace)",
            grace.as_secs()
        )
    } else {
        format!("doorbell: stopped (pid {pid})")
    })
}

fn restart_after_stop(
    stop: impl FnOnce() -> Result<()>,
    start: impl FnOnce() -> Result<()>,
) -> Result<()> {
    stop()?;
    start()
}

pub fn restart(cfg: &Config) -> Result<()> {
    restart_after_stop(|| stop(cfg, false), || start(cfg))
}

pub fn status(cfg: &Config) -> Result<()> {
    let local_pid = managed(cfg);
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
    let (alive, pid, pid_source) = status_liveness(local_pid, health.as_ref());
    let out = serde_json::json!({
        "alive": alive,
        "pid": pid,
        "pid_source": pid_source,
        "port": cfg.port,
        "socket": cfg.socket_path().display().to_string(),
        "health": health,
    });
    println!("{out}");
    Ok(())
}

/// Install the autostart unit (idempotent) and load it: macOS launchd
/// agent, Linux systemd user unit. Template placeholders take the live
/// The MCP URL the daemon hands its toron children through the unit env
/// (bd-jvu7): beside a live toron daemon, a child without it refuses to open
/// the store ("one store per project") and every sweep appends an
/// `inbox_error` audit row. Operator env wins; default = toron's documented
/// daemon port (docs/agent-instructions/backends.md).
fn toron_mcp_url() -> String {
    std::env::var("TORON_MCP_URL").unwrap_or_else(|_| "http://127.0.0.1:18080/mcp".to_string())
}

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
        &toron_mcp_url(),
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
    toron_mcp_url: &str,
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
            .replace("DOORBELL_TORON_MCP_URL_VALUE", toron_mcp_url)
            .replace("DOORBELL_TORON_MCP_URL_KEY", "TORON_MCP_URL")
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
            .replace("DOORBELL_TORON_MCP_URL_VALUE", toron_mcp_url)
            .replace("DOORBELL_TORON_MCP_URL_KEY", "TORON_MCP_URL")
            .replace("DOORBELL_SERVE_ARGS", &args.join(" "))
    }
}

/// The launchctl sequence that gets a rewritten unit file **actually
/// loaded**: cheap `bootstrap` (fresh installs never bootout), then `bootout`
/// to drop the stale loaded definition — `kickstart -k` restarts the OLD
/// definition, so file edits never reach an already-loaded job — then
/// `bootstrap` fresh, with `kickstart` last (wedged-domain recovery).
/// Shared by install and start so neither regresses into kickstart-on-stale.
fn reload_plan(uid: &str, unit: &std::path::Path) -> Vec<Vec<String>> {
    let domain = format!("gui/{uid}/com.doorbell.serve");
    let target = unit.display().to_string();
    vec![
        vec![
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{uid}"),
            target.clone(),
        ],
        vec!["launchctl".into(), "bootout".into(), domain.clone()],
        vec![
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{uid}"),
            target,
        ],
        vec!["launchctl".into(), "kickstart".into(), "-k".into(), domain],
    ]
}

/// Run the plan in order: the first bootstrap/kickstart success wins;
/// `bootout` never blocks the fresh load (an already-unloaded job is not
/// an error — bd-yvte's absence classifier decides that). Every real
/// failure's stderr is kept, so a bail names the actual launchctl cause
/// instead of just the step list. The world runs here, at the edge — the
/// decision fold below is pure and scripted in tests (effect law: no real
/// launchctl in a unit test).
fn run_reload_plan(plan: &[Vec<String>]) -> std::result::Result<(), String> {
    run_reload_plan_with(plan, |cmd| {
        match std::process::Command::new(&cmd[0]).args(&cmd[1..]).output() {
            Ok(o) => (
                o.status.success(),
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            ),
            Err(e) => (false, e.to_string()),
        }
    })
}

/// The decision fold over scripted step results (bd-rxyi): a REAL bootout
/// failure (not absence) means the stale loaded definition was never
/// dropped, so a later `kickstart -k` success restarts the OLD definition
/// and must not count as plan success — install would report success while
/// the daemon runs stale env (the bd-jvu7 misattribution class on the
/// install path). Kickstart counts only when no real drop was needed or
/// the drop succeeded. Absence stays benign: nothing to drop, nothing to
/// re-load. A real bootout failure also joins the all-fail bail — the old
/// best-effort `continue` dropped its cause from the list entirely.
fn run_reload_plan_with(
    plan: &[Vec<String>],
    mut exec: impl FnMut(&[String]) -> (bool, String),
) -> std::result::Result<(), String> {
    let mut errors: Vec<String> = Vec::new();
    let mut stale_drop_failed: Option<String> = None;
    for cmd in plan {
        let step = cmd.get(1).cloned().unwrap_or_default();
        let (ok, err) = exec(cmd);
        if step == "bootout" {
            if !ok && !bootout_absent(&err) {
                stale_drop_failed = Some(err.clone());
                errors.push(format!("{step}: {err}"));
            }
            continue; // never a blocker for the fresh-load arms
        }
        if ok {
            if step == "kickstart"
                && let Some(drop_err) = &stale_drop_failed
            {
                return Err(format!(
                    "launchctl reload plan failed: the stale loaded definition was never dropped ({drop_err}), so kickstart -k restarted the OLD definition — the rewritten unit file was not loaded; retry `doorbell install` or run the bootout manually"
                ));
            }
            return Ok(());
        }
        errors.push(format!("{step}: {err}"));
    }
    Err(format!(
        "launchctl reload plan failed: {}",
        errors.join(" | ")
    ))
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
        let domain = format!("gui/{uid}/com.doorbell.serve"); // bd-jvu7: rewriting the file is not loading it. The old fallback
        // (`kickstart -k`) restarted the LOADED definition, so a rewritten
        // plist (e.g. the toron MCP env) never reached a running job.
        if let Err(e) = run_reload_plan(&reload_plan(&uid, &path)) {
            anyhow::bail!("{e} for {domain}");
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
        let body = render_autostart(
            "/opt/doorbell",
            "/var/dbll",
            18980,
            "/var/fw",
            &hosts,
            "http://127.0.0.1:18080/mcp",
        );
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
        let body = render_autostart(
            "/opt/doorbell",
            "/var/dbll",
            18980,
            "/var/fw",
            &[],
            "http://127.0.0.1:18080/mcp",
        );
        assert!(!body.contains("--host"), "standby carries no --host");
        assert!(
            !body.contains("DOORBELL_SERVE_ARGS"),
            "no placeholder survives"
        );
    }

    /// bd-jvu7: the launchd env carried only `DOORBELL_STATE_DIR`, so every
    /// bd-jvu7's second half: `install` rewrites the plist, but launchd's
    /// `kickstart` restarts the **loaded** definition — file edits never
    /// reach an already-loaded job (the stale env kept refusing
    /// `toron mail inbox` forever while the file on disk looked correct).
    /// The reload plan must drop the stale definition, bootstrap fresh, and
    /// keep kickstart as last resort only.
    #[test]
    fn reload_plan_drops_the_stale_definition_before_rebootstrap() {
        let plist = "/Users/x/Library/LaunchAgents/com.doorbell.serve.plist";
        let plan = super::reload_plan("501", std::path::Path::new(plist));
        let flat: Vec<String> = plan.iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            flat.first().map(String::as_str),
            Some(format!("launchctl bootstrap gui/501 {plist}").as_str()),
            "cheap bootstrap first (fresh machines never bootout): {flat:?}"
        );
        let bootout = flat
            .iter()
            .position(|c| c.starts_with("launchctl bootout "))
            .expect("bootout must be in the plan — the stale definition is the bug");
        let rebootstrap = flat
            .iter()
            .rposition(|c| c.starts_with("launchctl bootstrap "))
            .expect("re-bootstrap after the drop");
        assert!(
            bootout < rebootstrap,
            "stale definition dropped before the fresh load: {flat:?}"
        );
        assert!(
            flat.last()
                .map(|c| c.starts_with("launchctl kickstart -k "))
                .unwrap_or(false),
            "kickstart only as last resort: {flat:?}"
        );
    }

    /// bd-rxyi: bootstrap fails (definition loaded) → bootout fails for a
    /// REAL reason (permission, not absence) → re-bootstrap fails ("already
    /// loaded") → `kickstart -k` restarts the OLD loaded definition and
    /// succeeds. The old plan returned Ok here: install reported success
    /// while the daemon ran the stale unit — the bd-jvu7 misattribution
    /// class on the install path. Kickstart success counts only when no
    /// real drop was needed or the drop succeeded.
    #[test]
    fn reload_plan_refuses_kickstart_success_after_a_failed_stale_drop() {
        let plan = super::reload_plan("501", std::path::Path::new("/tmp/p.plist"));
        let err = super::run_reload_plan_with(&plan, |cmd| match cmd.get(1).map(String::as_str) {
            Some("bootstrap") => (false, "service already loaded".into()),
            Some("bootout") => (false, "Boot-out failed: 1: Operation not permitted".into()),
            Some("kickstart") => (true, String::new()),
            _ => (false, "unexpected step".into()),
        })
        .expect_err("kickstart on a stale definition must not report plan success");
        assert!(
            err.contains("never dropped"),
            "explains the stale restart: {err}"
        );
        assert!(
            err.contains("Operation not permitted"),
            "names the real bootout cause: {err}"
        );
    }

    /// Absence is not a failed drop: bootout finding no job (manual start)
    /// leaves the plan's verdict exactly as before — a later kickstart
    /// success still counts (bd-yvte's absence classifier reused).
    #[test]
    fn reload_plan_bootout_absence_never_poisons_a_kickstart_recovery() {
        let plan = super::reload_plan("501", std::path::Path::new("/tmp/p.plist"));
        super::run_reload_plan_with(&plan, |cmd| match cmd.get(1).map(String::as_str) {
            Some("bootstrap") => (false, "transient busy".into()),
            Some("bootout") => (false, "Boot-out failed: 3: No such process".into()),
            Some("kickstart") => (true, String::new()),
            _ => (false, "unexpected step".into()),
        })
        .expect("absence means no drop was needed — kickstart success still wins");
    }

    /// First success wins and STOPS the walk: a fresh bootstrap (step 1)
    /// must not run bootout/kickstart after the new file is already
    /// loaded — no world touch after success.
    #[test]
    fn reload_plan_first_bootstrap_success_wins_and_stops_running_steps() {
        let plan = super::reload_plan("501", std::path::Path::new("/tmp/p.plist"));
        let mut ran: Vec<String> = Vec::new();
        super::run_reload_plan_with(&plan, |cmd| {
            ran.push(cmd.get(1).cloned().unwrap_or_default());
            (true, String::new())
        })
        .expect("fresh bootstrap succeeds");
        assert_eq!(
            ran,
            ["bootstrap"],
            "walk stops at the first success: {ran:?}"
        );
    }

    /// The all-fail bail keeps every real cause — including bootout's,
    /// which the old best-effort `continue` silently dropped from the
    /// error list even when nothing else succeeded either.
    #[test]
    fn reload_plan_final_bail_carries_the_real_bootout_cause() {
        let plan = super::reload_plan("501", std::path::Path::new("/tmp/p.plist"));
        let err = super::run_reload_plan_with(&plan, |cmd| match cmd.get(1).map(String::as_str) {
            Some("bootstrap") => (false, "service already loaded".into()),
            Some("bootout") => (false, "Boot-out failed: 1: Operation not permitted".into()),
            Some("kickstart") => (false, "could not find service".into()),
            _ => (false, "unexpected step".into()),
        })
        .expect_err("every step failing must bail");
        assert!(
            err.contains("bootout: Boot-out failed"),
            "bootout cause listed: {err}"
        );
        assert!(err.contains("kickstart"), "kickstart cause listed: {err}");
    }

    /// The `toron mail inbox` child refused beside the live toron daemon
    /// ("no daemon URL is configured … one store per project") and every
    /// sweep appended an `inbox_error` audit row — live rows at
    /// 2026-09-24T10:31Z, pins PlumGoose/BrightBadger. The unit must hand the
    /// daemon (and therefore every toron child it spawns) the MCP URL.
    #[test]
    fn rendered_unit_forwards_toron_mcp_url_to_the_daemon_env() {
        let body = render_autostart(
            "/opt/doorbell",
            "/var/dbll",
            18980,
            "/var/fw",
            &[],
            "http://127.0.0.1:18080/mcp",
        );
        assert!(
            body.contains("TORON_MCP_URL"),
            "the env name reaches the unit: {body}"
        );
        assert!(
            body.contains("http://127.0.0.1:18080/mcp"),
            "the URL value reaches the unit"
        );
        assert!(
            !body.contains("DOORBELL_TORON_MCP_URL_VALUE")
                && !body.contains("DOORBELL_TORON_MCP_URL_KEY"),
            "no placeholder survives"
        );
        #[cfg(target_os = "macos")]
        assert!(
            body.contains("<key>TORON_MCP_URL</key>"),
            "launchd env dict entry is well-formed"
        );
    }

    fn health(pid: u64) -> serde_json::Value {
        serde_json::json!({
            "protocol": doorbell_rt::api::PING.protocol,
            "owner": {"pid": pid},
        })
    }

    #[test]
    fn claim_pid_file_round_trips_through_read_and_release() {
        let dir = tmp_lifecycle("pidfile");
        let path = dir.join("doorbell.pid");
        claim_pid_file(&path, 42).unwrap();
        assert_eq!(read_pid(&path), Some(42));
        release_pid_file(&path, 42);
        assert!(!path.exists(), "release removes only our pidfile");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_propagates_stop_failure() {
        let result = restart_after_stop(
            || anyhow::bail!("stop failed"),
            || anyhow::bail!("start must not run after a failed stop"),
        );
        let error = result.expect_err("restart must fail when stop fails");
        assert!(error.to_string().contains("stop failed"));
    }

    fn test_cfg(dir: &std::path::Path, port: u16) -> Config {
        Config {
            state_dir: dir.to_path_buf(),
            port,
            flywheel_state: dir.join("flywheel"),
            hosts: Vec::new(),
        }
    }

    /// bd-doorbell-status-alive-h9hb. The two-caller mismatch, exactly as
    /// reproduced live on 2026-09-28 against ONE daemon on port 18980: same
    /// health body, opposite verdict, because `alive` was read off the
    /// caller's pid file. The second caller is the production shape —
    /// `flywheel doorbell status` passes the flywheel state dir, never the
    /// doorbell's.
    #[test]
    fn status_liveness_comes_from_the_probe_not_the_callers_pid_file() {
        let health = serde_json::json!({ "owner": { "pid": 7765 } });

        // Caller pointed at the daemon's own state dir: its pid file is here.
        let (alive, pid, source) = status_liveness(Some(7765), Some(&health));
        assert!(alive);
        assert_eq!(pid, Some(7765));
        assert_eq!(source, Some("pid_file"));

        // Caller pointed somewhere else, which is what flywheel does.
        let (alive, pid, source) = status_liveness(None, Some(&health));
        assert!(
            alive,
            "a daemon answering on the port is alive whatever the caller's pid file says"
        );
        assert_eq!(
            pid,
            Some(7765),
            "with no local pid file the pid falls back to the daemon's own report"
        );
        assert_eq!(source, Some("health"));
    }

    #[test]
    fn status_liveness_no_probe_means_not_alive_however_live_the_pid_file_looks() {
        // A pid file naming a live process is attribution, not proof of
        // service: we started something that is not answering.
        let (alive, pid, source) = status_liveness(Some(4242), None);
        assert!(!alive, "nothing answered, so nothing is alive");
        assert_eq!(
            pid,
            Some(4242),
            "the pid is still worth naming, because it is the one we can signal"
        );
        assert_eq!(source, Some("pid_file"));

        assert_eq!(status_liveness(None, None), (false, None, None));

        // A health body whose owner block carries no usable pid is still a
        // live daemon; it just has no pid to name.
        let bare = serde_json::json!({ "mode": "hosted" });
        assert_eq!(status_liveness(None, Some(&bare)), (true, None, None));
    }

    /// Spawn a child whose corpse gets reaped in the background. Production
    /// daemons are reparented to init/launchd (reaped on exit); a child we
    /// still hold would linger as a zombie, and `kill -0` succeeds on
    /// zombies — making a successfully stopped pid look unkillable.
    fn spawn_reaped(cmd: &mut std::process::Command) -> u32 {
        let child = cmd.spawn().unwrap();
        let pid = child.id();
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        pid
    }

    // ---- restart-storm regressions (bd-xwgh round 2) ----

    #[cfg(unix)]
    #[test]
    fn stop_reports_clean_sigterm_exit() {
        let dir = tmp_lifecycle("stop-term");
        let cfg = test_cfg(&dir, 0); // port 0: nothing can answer
        let mut sleep = std::process::Command::new("sleep");
        sleep.arg("300");
        let pid = spawn_reaped(&mut sleep);
        std::fs::write(cfg.pid_file(), format!("{pid}\n")).unwrap();
        let msg = stop_with_budgets(&cfg, false, Duration::from_secs(5), Duration::from_secs(2))
            .expect("SIGTERM-honouring pid must stop cleanly");
        assert!(msg.contains("stopped (pid"), "plain message: {msg}");
        assert!(!msg.contains("SIGKILL"), "no escalation expected: {msg}");
        assert!(!pid_alive(pid), "process must be gone");
        assert!(!cfg.pid_file().exists(), "pidfile cleared");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stop_escalates_to_sigkill_when_sigterm_is_ignored() {
        // `trap '' TERM` + exec: sleep inherits an ignored SIGTERM, so only
        // SIGKILL can end it. A 400ms budget proves the escalation path
        // without the real grace wait (budgets are injected, never ambient).
        let dir = tmp_lifecycle("stop-kill");
        let cfg = test_cfg(&dir, 0);
        let mut sh = std::process::Command::new("sh");
        sh.args(["-c", "trap '' TERM; exec sleep 300"]);
        let pid = spawn_reaped(&mut sh);
        std::fs::write(cfg.pid_file(), format!("{pid}\n")).unwrap();
        let msg = stop_with_budgets(
            &cfg,
            false,
            Duration::from_millis(400),
            Duration::from_secs(3),
        )
        .expect("SIGKILL escalation must still stop");
        assert!(msg.contains("SIGKILL"), "escalation reported: {msg}");
        assert!(!pid_alive(pid), "SIGKILLed process must be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stop_never_claims_success_when_the_port_survives() {
        let dir = tmp_lifecycle("stop-foreign");
        // A holder that accepts and immediately drops, so the health probe
        // fails fast instead of burning its 3s read timeout.
        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = holder.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in holder.incoming().flatten() {
                drop(conn);
            }
        });
        let cfg = test_cfg(&dir, port);
        let mut sleep = std::process::Command::new("sleep");
        sleep.arg("300");
        let pid = spawn_reaped(&mut sleep);
        std::fs::write(cfg.pid_file(), format!("{pid}\n")).unwrap();
        let err = stop_with_budgets(&cfg, false, Duration::from_secs(5), Duration::from_secs(2))
            .expect_err("a surviving endpoint must fail the stop");
        let text = err.to_string();
        assert!(text.contains("still held by another process"), "{text}");
        assert!(!pid_alive(pid), "our pid must still be terminated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_target_falls_back_to_the_live_health_owner() {
        use std::io::{Read, Write};
        // A pidfile-less daemon still identifies itself over /health —
        // stop must signal THAT pid instead of refusing as "unmanaged".
        let dir = tmp_lifecycle("stop-owner");
        let mut decoy = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = serde_json::json!({
            "protocol": doorbell_rt::api::PING.protocol,
            "owner": {"pid": decoy.id()},
        })
        .to_string();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        let mut buf = [0u8; 512];
                        let _ = s.read(&mut buf);
                        let _ = write!(
                            s,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        let cfg = test_cfg(&dir, port);
        assert_eq!(
            stop_target(&cfg),
            Some(decoy.id()),
            "pidfile-less stop must take the protocol-attested owner"
        );
        let _ = decoy.kill();
        let _ = decoy.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_failure_brief_carries_cause_and_log_tail() {
        let dir = tmp_lifecycle("start-brief");
        let log = dir.join("doorbell.log");
        std::fs::write(&log, "line one\nport 18980 held by non-doorbell squatter\n").unwrap();
        let brief = start_failure_brief(
            18980,
            "serve child (pid 68346)",
            "exited before /health answered (exit status: 1)",
            false,
            &log,
        );
        assert!(brief.contains("serve child (pid 68346)"), "{brief}");
        assert!(brief.contains("exit status: 1"), "{brief}");
        assert!(
            brief.contains("held by non-doorbell squatter"),
            "tail must surface the real cause: {brief}"
        );
        assert!(brief.contains("not answering"), "{brief}");
        let timed = start_failure_brief(18980, "serve child (pid 1)", "never answered", true, &log);
        assert!(timed.contains("ANSWERING now"), "{timed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_lines_reads_only_the_end() {
        let dir = tmp_lifecycle("tail");
        let log = dir.join("d.log");
        std::fs::write(&log, "a\nb\nc\nd\ne\n").unwrap();
        assert_eq!(tail_lines(&log, 2), vec!["d".to_string(), "e".to_string()]);
        assert!(tail_lines(&dir.join("missing.log"), 3).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scratch_state_dirs_never_unload_the_real_unit() {
        // A temp-dir `stop` (probe, test) must not boot out the loaded
        // service for a state dir the unit does not serve.
        let body = "<string>--state-dir</string><string>/Users/u/.local/state/doorbell</string>";
        assert!(unit_body_serves(
            body,
            std::path::Path::new("/Users/u/.local/state/doorbell")
        ));
        assert!(!unit_body_serves(
            body,
            std::path::Path::new("/tmp/dbcl-scratch")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn boot_probe_keeps_a_dead_child_visible_and_bounds_the_wait() {
        use std::os::unix::process::ExitStatusExt;
        let dead = std::process::ExitStatus::from_raw(9 << 8);
        assert!(
            matches!(boot_probe(Some(dead), true, false), Some(Boot::Exited(_))),
            "a dead child beats a peer answering /health"
        );
        assert_eq!(boot_probe(None, true, true), Some(Boot::Healthy));
        assert_eq!(boot_probe(None, false, true), Some(Boot::TimedOut));
        assert_eq!(boot_probe(None, false, false), None, "keep waiting");
    }

    /// Scripted stand-in for the process/endpoints/supervisor world: every
    /// observation is data, so these tests carry no real clock, sleep,
    /// signal, or socket (effect law: interpreter test by substitution).
    #[derive(Debug, Default)]
    struct ScriptedStop {
        target: Option<u32>,
        /// Answer for each `wait_dead` call: true = pid exits within budget.
        dead_after: std::collections::VecDeque<bool>,
        health: bool,
        tcp: bool,
        uds: bool,
        /// Leading `unload_supervisor` calls that fail (bd-yvte):
        /// decremented per call; 0 = the scripted world unloads cleanly.
        unload_fails: usize,
        actions: Vec<String>,
    }

    impl ScriptedStop {
        fn stop(&mut self, force: bool) -> Result<String> {
            stop_core(
                self,
                18980,
                std::path::Path::new("/tmp/scripted.sock"),
                force,
                Duration::from_secs(15),
                Duration::from_secs(5),
            )
        }
        fn position(&self, action: &str) -> usize {
            self.actions
                .iter()
                .position(|a| a == action)
                .unwrap_or_else(|| panic!("missing {action} in {:?}", self.actions))
        }
    }

    impl StopWorld for ScriptedStop {
        fn stop_target(&mut self) -> Option<u32> {
            self.target
        }
        fn signal(&mut self, pid: u32, sig: Sig) {
            self.actions.push(format!("signal {pid} {sig:?}"));
        }
        fn wait_dead(&mut self, pid: u32, grace: Duration) -> bool {
            self.actions.push(format!("wait {pid} {grace:?}"));
            self.dead_after.pop_front().unwrap_or(true)
        }
        fn unload_supervisor(&mut self) -> Result<(), String> {
            if self.unload_fails > 0 {
                self.unload_fails -= 1;
                self.actions.push("unload-failed".into());
                Err("scripted bootout failure".into())
            } else {
                self.actions.push("unload".into());
                Ok(())
            }
        }
        fn health_answers(&mut self) -> bool {
            self.health
        }
        fn tcp_alive(&mut self, _port: u16) -> bool {
            self.tcp
        }
        fn uds_alive(&mut self, _path: &std::path::Path) -> bool {
            self.uds
        }
        fn cleanup_state(&mut self, pid: u32) {
            self.actions.push(format!("cleanup {pid}"));
        }
        fn cleanup_stale(&mut self, _path: &std::path::Path) {
            self.actions.push("cleanup-stale".into());
        }
    }

    #[test]
    fn stop_core_signals_before_unloading_and_reports_the_clean_exit() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![true].into(),
            ..Default::default()
        };
        let msg = w.stop(false).expect("clean stop");
        assert_eq!(msg, "doorbell: stopped (pid 9)");
        // Load-bearing order: TERM is delivered BEFORE the supervisor
        // unload (bootout-first beheaded the drain — the restart bug).
        let term = w.position("signal 9 Term");
        let unload = w.position("unload");
        assert!(term < unload, "TERM must precede unload: {:?}", w.actions);
        assert!(
            !w.actions.iter().any(|a| a == "signal 9 Kill"),
            "{:?}",
            w.actions
        );
        assert_eq!(
            w.actions.last().map(String::as_str),
            Some("cleanup 9"),
            "{:?}",
            w.actions
        );
    }

    #[test]
    fn stop_core_escalates_a_hanging_drain_and_still_unloads() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![false, true].into(),
            ..Default::default()
        };
        let msg = w.stop(false).expect("escalated stop");
        assert!(msg.contains("SIGKILL"), "{msg}");
        let kill = w.position("signal 9 Kill");
        let unload = w.position("unload");
        // bd-pf3b flipped this ordering (respawn race): the job must be
        // gone before the unclean exit — escalation_unloads_before_sigkill
        // owns the full term < unload < kill contract.
        assert!(unload < kill, "unload before SIGKILL: {:?}", w.actions);
    }

    /// bd-pf3b: STOP_GRACE must cover the daemon's own contract — the full
    /// 10s drain, one liveness-probe window (200ms), and the post-drain
    /// exit path (flush, journal, load stretch = EXIT_PATH_ALLOWANCE).
    /// The old 15s lost this arithmetic under load: 5/5 stragglers rode
    /// the full drain, then SIGKILL plus the respawn race produced
    /// "port still held".
    #[test]
    fn stop_grace_covers_send_drain_probe_and_exit_path() {
        use crate::serve::{DRAIN_BUDGET, LIVENESS_PROBE_SAMPLE, SEND_BUDGET};
        let send = SEND_BUDGET;
        let drain = DRAIN_BUDGET;
        let probe = LIVENESS_PROBE_SAMPLE;
        let exit_path = super::EXIT_PATH_ALLOWANCE;
        assert!(
            super::STOP_GRACE >= send + drain + probe + exit_path,
            "STOP_GRACE {:?} < send {send:?} + drain {drain:?} + probe {probe:?} + exit {exit_path:?}",
            super::STOP_GRACE
        );
    }

    /// bd-pf3b's respawn race, inverted from the old ordering: SIGKILL is an
    /// unclean exit, and KeepAlive{SuccessfulExit=false} respawns it BEFORE
    /// a post-kill bootout runs — the respawn grabbed the port and stop
    /// bailed "still held by another process". Unload first (drop the job),
    /// then kill: the unclean exit finds nothing to respawn. The full grace
    /// still precedes the unload, so the drain keeps its budget.
    #[test]
    fn escalation_unloads_before_sigkill_so_keepalive_never_respawns() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![false, true].into(),
            ..Default::default()
        };
        w.stop(false).expect("escalated stop");
        let term = w.position("signal 9 Term");
        let unload = w.position("unload");
        let kill = w.position("signal 9 Kill");
        assert!(
            term < unload,
            "drain grace precedes unload: {:?}",
            w.actions
        );
        assert!(
            unload < kill,
            "job dropped before the unclean exit: {:?}",
            w.actions
        );
    }

    /// bd-yvte: the unload IS the respawn guard — swallowing its failure
    /// let SIGKILL resurrect the daemon and the endpoint bail then blamed
    /// "another process" for launchd's own respawn. Persistent bootout
    /// failure must stop the sequence BEFORE the kill, naming the real cause.
    #[test]
    fn escalation_bails_before_sigkill_when_bootout_persists_so_respawn_never_masks_the_cause() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![false, false].into(),
            unload_fails: 2,
            ..Default::default()
        };
        let err = w
            .stop(false)
            .expect_err("a dead respawn guard must refuse the kill");
        assert!(
            err.to_string().contains("supervisor unload failed twice"),
            "{err}"
        );
        assert!(
            !w.actions.iter().any(|a| a == "signal 9 Kill"),
            "SIGKILL with a loaded job = guaranteed respawn: {:?}",
            w.actions
        );
        assert_eq!(
            w.actions.iter().filter(|a| *a == "unload-failed").count(),
            2,
            "both bootout attempts observed: {:?}",
            w.actions
        );
    }

    /// Retry-once policy: a transient launchd busy must not fail the stop —
    /// one clean retry, then the normal kill path proceeds.
    #[test]
    fn escalation_retries_a_failed_bootout_once_before_killing() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![false, true].into(),
            unload_fails: 1,
            ..Default::default()
        };
        let msg = w.stop(false).expect("retry succeeds");
        assert!(msg.contains("SIGKILL"), "{msg}");
        let failed = w.position("unload-failed");
        let unload = w.position("unload");
        let kill = w.position("signal 9 Kill");
        assert!(
            failed < unload && unload < kill,
            "fail, retry, then kill: {:?}",
            w.actions
        );
    }

    /// Absence is success: bootout of a never-loaded job (manual start, no
    /// launchd job) must not block a stop — only a real failure does.
    #[test]
    fn bootout_absence_is_success_and_real_failures_are_not() {
        assert!(super::bootout_absent("Boot-out failed: 3: No such process"));
        assert!(super::bootout_absent(
            "Failed to stop unit: doorbell.service not loaded"
        ));
        assert!(!super::bootout_absent(
            "Boot-out failed: 1: Operation not permitted"
        ));
        assert!(super::bootout_absent(
            "Could not find service \"com.doorbell.serve\" in domain for user gui: 501"
        ));
    }

    /// Clean-arm policy pinned: a failing unload after a SIGTERM-honouring
    /// exit is warned loud, not fatal — the daemon IS stopped (a 0-exit is
    /// not respawned, so no misattribution is possible), and the arm makes
    /// exactly ONE attempt. Escalation owns retry-once-then-bail; a retry
    /// here would grow the success path for a stale-definition side effect
    /// `doorbell install` already heals via the reload plan.
    #[test]
    fn stop_succeeds_with_one_loud_unload_attempt_when_clean_exit_unload_fails() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![true].into(),
            unload_fails: 1,
            ..Default::default()
        };
        let msg = w.stop(false).expect("clean exit stays a successful stop");
        assert_eq!(msg, "doorbell: stopped (pid 9)");
        assert_eq!(
            w.actions.iter().filter(|a| *a == "unload-failed").count(),
            1,
            "clean path attempts once, no retry: {:?}",
            w.actions
        );
        assert!(
            !w.actions.iter().any(|a| a == "signal 9 Kill"),
            "clean exit never escalates: {:?}",
            w.actions
        );
        assert!(
            w.actions.iter().any(|a| a.starts_with("cleanup")),
            "the successful stop still cleans up: {:?}",
            w.actions
        );
    }

    #[test]
    fn stop_core_reports_an_unkillable_pid_instead_of_success() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![false, false].into(),
            ..Default::default()
        };
        let err = w.stop(false).expect_err("unkillable pid must fail");
        assert!(err.to_string().contains("survived SIGKILL"));
        assert!(
            w.actions.iter().any(|a| a == "unload"),
            "supervisor still unloaded on the hard-fail path: {:?}",
            w.actions
        );
        assert!(
            !w.actions.iter().any(|a| a.starts_with("cleanup")),
            "no success cleanup on failure: {:?}",
            w.actions
        );
    }

    #[test]
    fn stop_core_refuses_success_while_a_foreign_endpoint_survives() {
        let mut w = ScriptedStop {
            target: Some(9),
            dead_after: vec![true].into(),
            tcp: true,
            ..Default::default()
        };
        let err = w
            .stop(false)
            .expect_err("surviving endpoint must fail the stop");
        assert!(err.to_string().contains("still held by another process"));
        assert!(
            !w.actions.iter().any(|a| a.starts_with("cleanup")),
            "{:?}",
            w.actions
        );
    }

    #[test]
    fn stop_core_unmanaged_arms_refuse_loudly_and_never_touch_a_stranger() {
        // Busy port, no identifiable owner: refused without force…
        let mut busy = ScriptedStop {
            tcp: true,
            ..Default::default()
        };
        let err = busy.stop(false).expect_err("unmanaged holder");
        assert!(err.to_string().contains("unmanaged holder"));
        // …same when only /health answers (no owner attestation)…
        let mut via_health = ScriptedStop {
            health: true,
            ..Default::default()
        };
        let err = via_health.stop(false).expect_err("unmanaged holder");
        assert!(err.to_string().contains("unmanaged holder"));
        // …and --force still refuses, because it cannot identify a target.
        let mut busy_f = ScriptedStop {
            tcp: true,
            ..Default::default()
        };
        let err = busy_f.stop(true).expect_err("force cannot identify");
        assert!(
            err.to_string()
                .contains("cannot kill what it cannot identify")
        );
        // Nothing answers: idempotent already-stopped, stale state dropped.
        let mut idle = ScriptedStop::default();
        let msg = idle.stop(false).expect("already stopped");
        assert_eq!(msg, "doorbell: already stopped");
        assert!(
            idle.actions.iter().any(|a| a == "cleanup-stale"),
            "{:?}",
            idle.actions
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_unit_path_is_the_launchd_agent() {
        // Lockstep with install_rendered: drift here would silently turn
        // every supervised start into a detached fallback.
        let path = installed_unit_path().expect("macOS has a unit path");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("com.doorbell.serve.plist")
        );
        assert!(path.ends_with("Library/LaunchAgents/com.doorbell.serve.plist"));
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

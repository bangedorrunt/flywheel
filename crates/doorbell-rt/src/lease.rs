// governed-by: ADR-0012
//! Ownership lease + deference decision (D9, bd-asr2.3). Three layers keep
//! exactly one writer per workspace: the lease file, the bind fence (in
//! `doorbell-cli`), and the embedded deference ping. `WatcherGen` stays
//! the fourth fence. The flywheel spawn-path call site lands in bd-asr2.5;
//! the pure decision function and all three layers' tests land here.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Seconds without a heartbeat before a live-pid lease counts stale.
pub const LEASE_STALE_SECS: u64 = 90;

/// `state_dir/doorbell/owner-{workspace}.json`: who runs the loop for
/// that workspace right now. Per-workspace (D9: one writer PER workspace;
/// bd-ene2 fixes the Phase-1 global file that wrongly excluded a second
/// workspace's loop). Pre-per-ws `owner.json` files are orphaned, never read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerLease {
    pub pid: u32,
    pub boot_id: String,
    pub host: String,
    pub started_unix: u64,
    pub heartbeat_unix: u64,
    /// Stable per-process start stamp (unix secs) of the holder.
    /// Legacy files lack it (0) and read stale immediately.
    #[serde(default)]
    pub process_start_unix: u64,
    /// What the holder does with the workspace. `None` = a ringing writer
    /// (the legacy lease and the embedded host). [`KIND_NO_TEXT`] = a daemon
    /// host under the comms flag: it holds the workspace's journal and
    /// brain but types nothing, so it is not "the ringer" (follow-up a).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// The lease kind of a daemon host that types nothing (comms flag on).
pub const KIND_NO_TEXT: &str = "no-text";

/// Does the holder of `lease` ring panes? A no-text daemon host does not, so
/// a sender must not defer to it and `flywheel __doorbell` may run its inbox
/// host beside it.
pub fn is_ringer(lease: &OwnerLease) -> bool {
    lease.kind.as_deref() != Some(KIND_NO_TEXT)
}

/// The lease key of the INBOX host (the pane writer under the comms flag) for
/// `workspace`: a second lease, so two inbox hosts cannot ring one workspace
/// even while a no-text daemon holds the workspace lease itself.
pub fn inbox_lease_key(workspace: &str) -> String {
    format!("inbox-{workspace}")
}
pub fn owner_file(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("owner-{workspace}.json"))
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn boot_id() -> String {
    if let Some(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
    {
        return id;
    }
    if let Some(id) = macos_boot_id() {
        return id;
    }
    if let Some(btime) = linux_btime() {
        return format!("boot-{btime}");
    }
    format!("unknown-{}", host_name())
}

fn macos_boot_id() -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let sec = s.split("sec = ").nth(1)?.split(',').next()?.trim();
    let sec: u64 = sec.parse().ok()?;
    Some(format!("boot-{sec}"))
}

fn linux_btime() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    for line in stat.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

pub fn host_name() -> String {
    if let Some(h) = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
    {
        return h;
    }
    if let Some(h) = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return h;
    }
    "localhost".into()
}

/// Stable start stamp (unix secs) for `pid`, or `None` when the platform
/// cannot answer. Linux reads `/proc`; macOS parses `ps -o lstart=`.
pub fn process_start_unix(pid: u32) -> Option<u64> {
    if pid <= 1 {
        return None;
    }
    if let Some(t) = linux_process_start(pid) {
        return Some(t);
    }
    macos_process_start(pid)
}

fn linux_process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let starttime: u64 = fields.get(19)?.parse().ok()?;
    let btime = linux_btime()?;
    // SAFETY: sysconf with _SC_CLK_TCK only reads a system constant.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Some(btime + starttime / ticks)
}

fn macos_process_start(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(&s, "%a %b %e %H:%M:%S %Y")
        .ok()
        .and_then(|naive| naive.and_local_timezone(chrono::Local).single())
        .map(|dt| dt.timestamp() as u64)
}

pub fn pid_alive(pid: u32) -> bool {
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

pub fn read_lease(state_dir: &Path, workspace: &str) -> Option<OwnerLease> {
    let raw = std::fs::read_to_string(owner_file(state_dir, workspace)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Live (non-stale) leases across workspaces — the ws-blind deference
/// probe (flywheel `resolve_host` has no workspace yet at decide time).
pub fn live_leases(state_dir: &Path, now_unix: u64) -> Vec<(String, OwnerLease)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(state_dir.join("doorbell")) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(ws) = name
            .strip_prefix("owner-")
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if ws.is_empty() {
            continue;
        }
        if let Some(lease) = read_lease(state_dir, ws)
            && !lease_stale(&lease, now_unix)
        {
            out.push((ws.to_string(), lease));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Injected observations for the pure staleness predicate: no clock, no
/// spawn, no ambient reads inside the fold.
pub struct LeaseInputs<'a> {
    pub current_boot: &'a str,
    pub pid_alive: bool,
    pub pid_start: Option<u64>,
}

/// Pure staleness: different boot or mismatched start is stale now; a live
/// pid on the same boot still needs a fresh heartbeat.
pub fn lease_stale_pure(lease: &OwnerLease, now_unix: u64, inputs: &LeaseInputs) -> bool {
    if lease.boot_id != inputs.current_boot {
        return true;
    }
    if lease.process_start_unix == 0 {
        return true;
    }
    if !inputs.pid_alive {
        return true;
    }
    if let Some(start) = inputs.pid_start
        && start != lease.process_start_unix
    {
        return true;
    }
    now_unix.saturating_sub(lease.heartbeat_unix) > LEASE_STALE_SECS
}

/// Stale = different boot OR pid dead OR start mismatch OR heartbeat older
/// than 90s. Edge: cheap checks first (heartbeat age, boot, legacy stamp
/// need no subprocess), then pid liveness and start stamp only when those
/// pass — the supervisor calls this for every known workspace each heal
/// tick, and the common dead/stale verdict must not cost a `ps` spawn.
/// The verdict still goes through [`lease_stale_pure`].
pub fn lease_stale(lease: &OwnerLease, now_unix: u64) -> bool {
    if now_unix.saturating_sub(lease.heartbeat_unix) > LEASE_STALE_SECS {
        return true;
    }
    let current_boot = boot_id();
    if lease.boot_id != current_boot {
        return true;
    }
    if lease.process_start_unix == 0 {
        return true;
    }
    let alive = pid_alive(lease.pid);
    let start = process_start_unix(lease.pid);
    lease_stale_pure(
        lease,
        now_unix,
        &LeaseInputs {
            current_boot: &current_boot,
            pid_alive: alive,
            pid_start: start,
        },
    )
}

/// Writer side: claim the lease (fails when a live lease holds it) and
/// refresh the heartbeat every sweep. Same-host pid comparison only —
/// cross-host fleets revisit this in the federation ADR (D10).
pub fn acquire_lease(
    state_dir: &Path,
    workspace: &str,
    now_unix: u64,
) -> Result<OwnerLease, LeaseDenied> {
    acquire_lease_kind(state_dir, workspace, now_unix, None)
}

/// [`acquire_lease`] recording what the holder does ([`KIND_NO_TEXT`]).
pub fn acquire_lease_kind(
    state_dir: &Path,
    workspace: &str,
    now_unix: u64,
    kind: Option<&str>,
) -> Result<OwnerLease, LeaseDenied> {
    if let Some(cur) = read_lease(state_dir, workspace)
        && !lease_stale(&cur, now_unix)
    {
        return Err(LeaseDenied::Held(cur));
    }
    let lease = OwnerLease {
        pid: std::process::id(),
        boot_id: boot_id(),
        host: host_name(),
        started_unix: now_unix,
        heartbeat_unix: now_unix,
        process_start_unix: process_start_unix(std::process::id()).unwrap_or(now_unix),
        kind: kind.map(str::to_string),
    };
    write_lease(state_dir, workspace, &lease)?;
    Ok(lease)
}

pub fn refresh_lease(
    state_dir: &Path,
    workspace: &str,
    lease: &mut OwnerLease,
    now_unix: u64,
) -> bool {
    lease.heartbeat_unix = now_unix;
    write_lease(state_dir, workspace, lease).is_ok()
}

pub fn release_lease(state_dir: &Path, workspace: &str, lease: &OwnerLease) {
    match read_lease(state_dir, workspace) {
        Some(cur)
            if cur.pid == lease.pid
                && cur.boot_id == lease.boot_id
                && (cur.process_start_unix == 0
                    || lease.process_start_unix == 0
                    || cur.process_start_unix == lease.process_start_unix) =>
        {
            let _ = std::fs::remove_file(owner_file(state_dir, workspace));
        }
        _ => {}
    }
}

fn write_lease(state_dir: &Path, workspace: &str, lease: &OwnerLease) -> Result<(), LeaseDenied> {
    let path = owner_file(state_dir, workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| LeaseDenied::Io)?;
    }
    serde_json::to_string(lease)
        .map_err(|_| LeaseDenied::Io)
        .and_then(|json| std::fs::write(path, json).map_err(|_| LeaseDenied::Io))
}

#[derive(Debug)]
pub enum LeaseDenied {
    Held(OwnerLease),
    Io,
}

/// Embedded deference decision (pure; the flywheel spawn path calls this
/// in bd-asr2.5 after its 200ms UDS probe — D9 layer 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deference {
    /// Daemon alive and healthy: attach as API client, never run a loop.
    DeferToDaemon,
    /// No lease or stale lease: take it and run embedded (degraded mode).
    TakeLease,
    /// Live lease held by another writer: refuse to start (no double run).
    RefuseDoubleRun,
}

pub fn deference_decision(
    lease: Option<&OwnerLease>,
    daemon_healthy: bool,
    now_unix: u64,
) -> Deference {
    if daemon_healthy {
        return Deference::DeferToDaemon;
    }
    match lease {
        None => Deference::TakeLease,
        Some(l) if lease_stale(l, now_unix) => Deference::TakeLease,
        Some(_) => Deference::RefuseDoubleRun,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lease_kind_round_trips_and_a_legacy_file_is_a_ringer() {
        let d = std::env::temp_dir().join(format!("dbrt-lease-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let now = unix_now();
        let held = acquire_lease_kind(&d, "w", now, Some(KIND_NO_TEXT)).unwrap();
        let read = read_lease(&d, "w").unwrap();
        assert_eq!(read.kind.as_deref(), Some(KIND_NO_TEXT));
        assert!(!is_ringer(&read));
        release_lease(&d, "w", &held);
        std::fs::create_dir_all(d.join("doorbell")).unwrap();
        std::fs::write(
            owner_file(&d, "w"),
            r#"{"pid":1,"boot_id":"b","host":"h","started_unix":1,"heartbeat_unix":1}"#,
        )
        .unwrap();
        let legacy = read_lease(&d, "w").unwrap();
        assert!(legacy.kind.is_none() && is_ringer(&legacy));
        release_lease(&d, "w", &legacy);
        let _ = std::fs::remove_file(owner_file(&d, "w"));
        let plain = acquire_lease(&d, "w", now).unwrap();
        assert!(
            !std::fs::read_to_string(owner_file(&d, "w"))
                .unwrap()
                .contains("kind")
        );
        assert_eq!(inbox_lease_key("w"), "inbox-w");
        release_lease(&d, "w", &plain);
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbrt-lease-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn acquire_heartbeat_takeover_cycle() {
        let d = dir("cycle");
        let now = unix_now();
        let mut lease = acquire_lease(&d, "w", now).expect("first acquire");
        assert!(matches!(
            acquire_lease(&d, "w", now),
            Err(LeaseDenied::Held(_))
        ));
        assert!(acquire_lease(&d, "x", now).is_ok());
        assert!(refresh_lease(&d, "w", &mut lease, now + 10));
        assert_eq!(read_lease(&d, "w").unwrap().heartbeat_unix, now + 10);
        let mut dead = lease.clone();
        dead.pid = 999_999_999;
        dead.heartbeat_unix = now;
        write_lease(&d, "w", &dead).unwrap();
        assert!(lease_stale(&dead, now + 1));
        let next = acquire_lease(&d, "w", now + 1).expect("stale takeover");
        assert_eq!(next.pid, std::process::id());
        release_lease(&d, "w", &next);
        assert!(read_lease(&d, "w").is_none());
        assert_eq!(live_leases(&d, now + 10).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deference_matrix() {
        let now = unix_now();
        assert_eq!(deference_decision(None, false, now), Deference::TakeLease);
        assert_eq!(
            deference_decision(None, true, now),
            Deference::DeferToDaemon
        );
        let live = OwnerLease {
            pid: std::process::id(),
            boot_id: boot_id(),
            host: host_name(),
            started_unix: now,
            heartbeat_unix: now,
            process_start_unix: process_start_unix(std::process::id()).unwrap_or(now),
            kind: None,
        };
        assert_eq!(
            deference_decision(Some(&live), true, now),
            Deference::DeferToDaemon
        );
        assert_eq!(
            deference_decision(Some(&live), false, now),
            Deference::RefuseDoubleRun
        );
        let stale = OwnerLease {
            heartbeat_unix: now - LEASE_STALE_SECS - 1,
            ..live
        };
        assert!(lease_stale(&stale, now));
    }

    /// bd-lease-boot-identity-tm6cg: dead pid is stale; a reused pid with a
    /// different start stamp is stale now; a live holder with a fresh
    /// heartbeat stays. Pure predicate over injected inputs, no sleep.
    #[test]
    fn boot_and_start_identity_decide_staleness() {
        let now = unix_now();
        let boot = boot_id();
        let me = std::process::id();
        let my_start = process_start_unix(me).unwrap_or(now);
        let live = OwnerLease {
            pid: me,
            boot_id: boot.clone(),
            host: host_name(),
            started_unix: now,
            heartbeat_unix: now,
            process_start_unix: my_start,
            kind: None,
        };
        let live_inputs = LeaseInputs {
            current_boot: &boot,
            pid_alive: true,
            pid_start: Some(my_start),
        };
        assert!(!lease_stale_pure(&live, now, &live_inputs));
        let dead_inputs = LeaseInputs {
            current_boot: &boot,
            pid_alive: false,
            pid_start: None,
        };
        assert!(lease_stale_pure(&live, now, &dead_inputs));
        let reused_inputs = LeaseInputs {
            current_boot: &boot,
            pid_alive: true,
            pid_start: Some(my_start.wrapping_add(3600)),
        };
        assert!(lease_stale_pure(&live, now, &reused_inputs));
        let other_inputs = LeaseInputs {
            current_boot: "boot-0",
            pid_alive: true,
            pid_start: Some(my_start),
        };
        assert!(lease_stale_pure(&live, now, &other_inputs));
        let legacy = OwnerLease {
            process_start_unix: 0,
            ..live.clone()
        };
        assert!(lease_stale_pure(&legacy, now, &live_inputs));
        assert!(!lease_stale(&live, now));
    }

    /// bd-lease-stale-hotpath-awm4z: the cheap path decides stale without
    /// a start stamp. Heartbeat older than the window, or a boot mismatch
    /// on a fresh heartbeat, is stale with `pid_start: None` — no `ps`.
    #[test]
    fn cheap_checks_decide_stale_without_a_start_stamp() {
        let now = unix_now();
        let boot = boot_id();
        let no_start = LeaseInputs {
            current_boot: &boot,
            pid_alive: true,
            pid_start: None,
        };
        let old = OwnerLease {
            pid: std::process::id(),
            boot_id: boot.clone(),
            host: host_name(),
            started_unix: now - LEASE_STALE_SECS - 10,
            heartbeat_unix: now - LEASE_STALE_SECS - 10,
            process_start_unix: 12345,
            kind: None,
        };
        assert!(lease_stale_pure(&old, now, &no_start));
        let fresh = OwnerLease {
            heartbeat_unix: now,
            started_unix: now,
            ..old.clone()
        };
        let other_boot = LeaseInputs {
            current_boot: "boot-0",
            pid_alive: true,
            pid_start: None,
        };
        assert!(lease_stale_pure(&fresh, now, &other_boot));
        assert!(!lease_stale_pure(&fresh, now, &no_start));
    }
}

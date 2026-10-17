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
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("unknown-{}", host_name()))
}

pub fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
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

/// Stale = pid dead OR heartbeat older than 90s (either suffices; a live
/// pid with a stuck heartbeat is a wedged writer, not an owner).
pub fn lease_stale(lease: &OwnerLease, now_unix: u64) -> bool {
    if !pid_alive(lease.pid) {
        return true;
    }
    now_unix.saturating_sub(lease.heartbeat_unix) > LEASE_STALE_SECS
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
    // Only release our own claim (pid + boot match); never drop a stranger.
    match read_lease(state_dir, workspace) {
        Some(cur) if cur.pid == lease.pid && cur.boot_id == lease.boot_id => {
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
        // A lease written before `kind` existed has no such field: a ringer.
        std::fs::create_dir_all(d.join("doorbell")).unwrap();
        std::fs::write(
            owner_file(&d, "w"),
            r#"{"pid":1,"boot_id":"b","host":"h","started_unix":1,"heartbeat_unix":1}"#,
        )
        .unwrap();
        let legacy = read_lease(&d, "w").unwrap();
        assert!(legacy.kind.is_none() && is_ringer(&legacy));
        // The plain acquire writes no kind at all.
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
        // Second acquire while live refuses.
        assert!(matches!(
            acquire_lease(&d, "w", now),
            Err(LeaseDenied::Held(_))
        ));
        // A second workspace is independent (bd-ene2 per-ws scope).
        assert!(acquire_lease(&d, "x", now).is_ok());
        // Heartbeat refreshes.
        assert!(refresh_lease(&d, "w", &mut lease, now + 10));
        assert_eq!(read_lease(&d, "w").unwrap().heartbeat_unix, now + 10);
        // Dead-pid lease is stale: takeover succeeds.
        let mut dead = lease.clone();
        dead.pid = 999_999_999;
        dead.heartbeat_unix = now;
        write_lease(&d, "w", &dead).unwrap();
        assert!(lease_stale(&dead, now + 1));
        let next = acquire_lease(&d, "w", now + 1).expect("stale takeover");
        assert_eq!(next.pid, std::process::id());
        // Release drops our own claim only.
        release_lease(&d, "w", &next);
        assert!(read_lease(&d, "w").is_none());
        // live_leases sees the surviving workspace claim only.
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
        // Live pid but stuck heartbeat still counts stale (wedged writer).
        assert!(lease_stale(&stale, now));
    }
}

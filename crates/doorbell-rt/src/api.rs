// governed-by: ADR-0012
//! Ask verbs (D8): request + reply channel + timeout, errors as typed
//! values — the CLI renders "daemon wedged" instead of hanging.

use crate::msg::{AskKind, AskReply, BrainMsg};
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot};

/// Ask taxonomy (kameo's `SendError` adapted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    /// Brain alive but too slow (timeout).
    Busy,
    /// Brain gone (channel closed).
    Absent,
    /// Brain alive but wedged (last sweep too old — reported by stats).
    Wedged { last_sweep_age_s: u64 },
    /// Verb refused by the authz matrix (D6; enforced at the API edge).
    Refused(String),
}

impl std::fmt::Display for AskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskError::Busy => write!(f, "daemon busy (ask timed out)"),
            AskError::Absent => write!(f, "daemon absent (brain channel closed)"),
            AskError::Wedged { last_sweep_age_s } => {
                write!(f, "daemon wedged (last sweep {last_sweep_age_s}s ago)")
            }
            AskError::Refused(reason) => write!(f, "refused: {reason}"),
        }
    }
}

/// Counters that make the wake SLA measurable (D3 shed counts; reconcile
/// counts ride here when the gap reconciler lands).
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub rings_by_class: BTreeMap<String, u64>,
    pub injects: u64,
    pub drops: u64,
    pub sheds: u64,
    pub deaf: u64,
    pub digest_batches: u64,
    pub sweep_lag_s: u64,
    pub degraded: u64,
    /// Retained journal rows visible to this snapshot (journal lag
    /// denominator: a growing gap between rows and replay position).
    pub journal_rows: u64,
}

/// bd-k0p7: the build identity of the RUNNING binary, so `/health` can name
/// which build is actually answering instead of only naming a version
/// constant that every build of every age shares.
///
/// `core_v` is a compiled-in constant, so a stale installed binary and a
/// freshly built one are byte-identical in `/health` until you go looking at
/// file mtimes. Observed live: a committed fix to `crates/doorbell-cli` sat
/// unrun for 11 hours because `flywheel` was rebuilt and `doorbell` was not,
/// and only noticing the stamp file was 8.9h old caught it.
///
/// Fields are Option because "I could not determine this" must read as
/// absent, never as a zero that looks like a real value. `age_s` is measured
/// from the binary's mtime to now, so an operator sees the age directly
/// rather than having to subtract timestamps by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BuildStamp {
    /// Package version of the running binary.
    pub version: String,
    /// The running executable's path, when resolvable.
    pub exe: Option<String>,
    /// The executable's mtime, unix seconds.
    pub exe_mtime_unix: Option<u64>,
    /// Seconds between that mtime and the moment `/health` was built. `None`
    /// when the mtime is unknown or is in the future (clock skew, which must
    /// not read as a negative or absurd age).
    pub age_s: Option<u64>,
    /// The binary's length in bytes — a second identity axis, so a same-mtime
    /// rebuild of a different size is still distinguishable.
    pub exe_len: Option<u64>,
}

impl BuildStamp {
    /// The compiled-in half. Always present, even when the executable cannot
    /// be inspected, so the stamp is never wholly empty.
    pub fn compiled() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            ..Self::default()
        }
    }
}

/// The decision half of the build stamp, kept PURE so the mtime/now/size
/// triple is testable without touching the filesystem (bd-k0p7).
///
/// `None` for the observed fields whenever the identity could not be read, so
/// "unknown" is representable and a zero is never mistaken for an answer.
pub fn resolve_build_stamp(observed: Option<(u64, u64)>, now_unix: u64) -> BuildStamp {
    let mut stamp = BuildStamp::compiled();
    let Some((mtime, len)) = observed else {
        return stamp;
    };
    stamp.exe_mtime_unix = Some(mtime);
    stamp.exe_len = Some(len);
    // A future mtime means clock skew or a touched file. Report no age rather
    // than a wrapped-around u64 that would read as a decades-old build.
    stamp.age_s = now_unix.checked_sub(mtime);
    stamp
}

/// Version handshake payload (D8): skew refuses loudly, never mis-renders.
#[derive(Debug, Clone)]
pub struct Ping {
    pub protocol: &'static str,
    pub core_v: &'static str,
    pub journal_v: u64,
}

pub const PING: Ping = Ping {
    protocol: "doorbell.v1",
    core_v: "doorbell-core.v1",
    journal_v: crate::JOURNAL_V,
};

/// Ask the brain; timeout is the caller's SLA, not the brain's.
pub async fn ask(
    brain_tx: &mpsc::Sender<BrainMsg>,
    kind: AskKind,
    timeout: std::time::Duration,
) -> Result<AskReply, AskError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    brain_tx
        .send(BrainMsg::Ask {
            kind,
            reply: reply_tx,
        })
        .await
        .map_err(|_| AskError::Absent)?;
    tokio::time::timeout(timeout, reply_rx)
        .await
        .map_err(|_| AskError::Busy)?
        .map_err(|_| AskError::Absent)
}

/// Handshake compatibility (D8/E16): a stale CLI against a new daemon
/// names both versions instead of mis-rendering. Versions only move
/// forward; anything unequal refuses loudly.
pub fn check_compat(peer_core_v: &str, peer_journal_v: u64) -> Result<(), String> {
    check_compat_with(PING.core_v, PING.journal_v, peer_core_v, peer_journal_v)
}

/// Explicit four-way form: the caller's pinned versions against the peer's.
pub fn check_compat_with(
    want_core_v: &str,
    want_journal_v: u64,
    peer_core_v: &str,
    peer_journal_v: u64,
) -> Result<(), String> {
    if peer_core_v != want_core_v {
        return Err(format!(
            "version skew: peer core_v={peer_core_v} but this side expects {want_core_v}"
        ));
    }
    if peer_journal_v != want_journal_v {
        return Err(format!(
            "version skew: peer journal_v={peer_journal_v} but this side expects {want_journal_v}"
        ));
    }
    Ok(())
}
/// One SSE event: journal row (or notice) with a replayable id.
/// `id` is the journal seq (0 for untagged legacy rows, ordered by fold).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub id: u64,
    pub kind: String,
    pub data: String,
}

impl SseEvent {
    pub fn format(&self) -> String {
        format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.id, self.kind, self.data
        )
    }

    /// Parse a `Last-Event-ID` header value (plain seq).
    pub fn parse_last_id(header: &str) -> Option<u64> {
        header.trim().parse().ok()
    }
}

/// Replay outcome for a reconnect (D8): gap-free events, or an explicit
/// first-replayable-seq notice when the cursor predates retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay {
    Events(Vec<SseEvent>),
    Gone { first_replayable: Option<u64> },
}

/// Replay journal rows after `last_id` (exclusive). Rows without seq sort
/// by fold order with id 0 and always replay (legacy). When `last_id`
/// predates the first retained seq, say so instead of a partial stream.
pub fn replay_range(
    rows: &[serde_json::Value],
    first_seq: Option<u64>,
    last_id: Option<u64>,
) -> Replay {
    if let (Some(cursor), Some(first)) = (last_id, first_seq)
        && cursor < first.saturating_sub(1)
    {
        return Replay::Gone {
            first_replayable: first_seq,
        };
    }
    let mut out = Vec::new();
    for row in rows {
        let seq = row.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        if last_id.is_some_and(|c| seq != 0 && seq <= c) {
            continue;
        }
        let kind = row
            .get("executed")
            .and_then(|e| e.get("stage"))
            .and_then(|s| s.as_str())
            .unwrap_or("wake")
            .to_string();
        out.push(SseEvent {
            id: seq,
            kind,
            data: row.to_string(),
        });
    }
    Replay::Events(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_gap_free_or_names_first_replayable() {
        let rows: Vec<serde_json::Value> = (10u64..=12)
            .map(|s| serde_json::json!({"seq": s, "executed": {"stage": "decided"}}))
            .collect();
        // Fresh consumer replays everything.
        let Replay::Events(all) = replay_range(&rows, Some(10), None) else {
            panic!("fresh replay must stream");
        };
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, 10);
        // Reconnect at 11 resumes gap-free from 12.
        let Replay::Events(tail) = replay_range(&rows, Some(10), Some(11)) else {
            panic!("reconnect must resume");
        };
        assert_eq!(tail.iter().map(|e| e.id).collect::<Vec<_>>(), vec![12]);
        // Cursor predating retention says the first replayable seq.
        assert!(matches!(
            replay_range(&rows, Some(10), Some(5)),
            Replay::Gone {
                first_replayable: Some(10)
            }
        ));
    }

    #[test]
    fn handshake_skew_names_both_versions() {
        assert!(check_compat("doorbell-core.v1", 2).is_ok());
        let err = check_compat("doorbell-core.v0", 2).unwrap_err();
        assert!(
            err.contains("doorbell-core.v1") && err.contains("v0"),
            "{err}"
        );
        let err = check_compat("doorbell-core.v1", 1).unwrap_err();
        assert!(err.contains('2') && err.contains('1'), "{err}");
    }

    /// bd-k0p7: the stamp must NAME the build, so it carries a version, an
    /// age, and a second identity axis. A stamp with only `core_v` is what
    /// made a stale binary indistinguishable from a fresh one.
    #[test]
    fn doorbell_health_build_stamp_names_the_running_build() {
        let now = 1_000_000u64;
        // The exact incident shape: built 8.9h ago and still answering.
        let eight_nine_h = 32_040u64;
        let stamp = resolve_build_stamp(Some((now - eight_nine_h, 4096)), now);
        assert_eq!(stamp.age_s, Some(eight_nine_h), "age is reported");
        assert_eq!(stamp.exe_mtime_unix, Some(now - eight_nine_h));
        assert_eq!(stamp.exe_len, Some(4096), "size is a second identity axis");
        assert!(
            !stamp.version.is_empty(),
            "a stamp with no version names no build"
        );
        // Serializes, so `/health` can actually carry it.
        let json = serde_json::to_string(&stamp).expect("the stamp serializes");
        assert!(json.contains("\"age_s\""), "age is in the payload: {json}");
    }

    /// bd-k0p7 unhappy path: an unreadable exe must read as ABSENT, never as
    /// a zero that looks like a real mtime/age/size.
    #[test]
    fn doorbell_health_build_stamp_reports_unknown_as_absent() {
        let now = 1_000_000u64;
        let stamp = resolve_build_stamp(None, now);
        assert_eq!(stamp.exe_mtime_unix, None);
        assert_eq!(stamp.exe_len, None);
        assert_eq!(stamp.age_s, None, "unknown must not read as age 0");
        assert!(
            !stamp.version.is_empty(),
            "the compiled-in half survives an unreadable exe"
        );
    }

    /// bd-k0p7 unhappy path: a future mtime is clock skew. `checked_sub`
    /// must yield no age rather than wrapping to a ~584-billion-year build.
    #[test]
    fn doorbell_health_build_stamp_refuses_a_future_mtime() {
        let now = 1_000_000u64;
        let stamp = resolve_build_stamp(Some((now + 500, 10)), now);
        assert_eq!(stamp.age_s, None, "skew yields no age, not a wrap");
        assert_eq!(
            stamp.exe_mtime_unix,
            Some(now + 500),
            "the mtime is still reported"
        );
    }

    /// bd-k0p7: two builds of the same version must be DISTINGUISHABLE. This
    /// is the whole defect — `core_v` alone cannot tell them apart.
    #[test]
    fn doorbell_health_build_stamp_separates_two_builds_of_one_version() {
        let now = 1_000_000u64;
        let older = resolve_build_stamp(Some((now - 32_040, 4096)), now);
        let newer = resolve_build_stamp(Some((now - 60, 4096)), now);
        assert_eq!(older.version, newer.version, "same version constant");
        assert_ne!(older.age_s, newer.age_s, "the age is what separates them");
        // Same-second rebuild of a DIFFERENT size is still separable.
        let resized = resolve_build_stamp(Some((now - 60, 8192)), now);
        assert_ne!(
            newer.exe_len, resized.exe_len,
            "size separates a same-second rebuild"
        );
    }

    /// bd-k0p7: the /health BODY must actually carry the stamp. The pure
    /// resolver being correct proves nothing if the payload omits it, which
    /// is exactly how `core_v` looked sufficient for so long.
    #[test]
    fn doorbell_health_body_carries_the_build_stamp() {
        let stamp = crate::api::resolve_build_stamp(Some((1_000, 42)), 2_000);
        let body = serde_json::json!({
            "protocol": "doorbell.v1",
            "core_v": "doorbell-core.v1",
            "build": stamp,
        })
        .to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("health is json");
        let build = parsed.get("build").expect("health carries a build key");
        assert_eq!(build["age_s"], 1_000, "age is in the body: {body}");
        assert_eq!(build["exe_len"], 42);
        assert!(
            build["version"].as_str().is_some_and(|v| !v.is_empty()),
            "the body names a version: {body}"
        );
    }
}

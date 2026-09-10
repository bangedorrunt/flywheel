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
}

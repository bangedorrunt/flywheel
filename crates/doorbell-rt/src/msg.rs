// governed-by: ADR-0012
//! [`BrainMsg`] — every state change enters the brain as a message (D3).
//! Wall-clock enters only as payloads (replayable); pacing uses monotonic time.

/// All inputs to the single-writer brain. Sensor tasks `tell` these;
/// CLI/API verbs `ask` via [`crate::api::ask`]. Never awaits I/O inside.
#[derive(Debug)]
pub enum BrainMsg {
    /// Plane-S watch NDJSON line (`{kind,id,tags,content,created_at}`).
    Watch(String),
    /// Herdr pane status change for one pin.
    HerdrStatus { pin: String, status: String },
    /// Watch child died; supervisor must restart, never exit (GS-007).
    Died,
    /// 1s sweep beat (`since` RFC3339, `now` unix secs).
    Sweep { now: u64 },
    /// Role bind drained from the pending queue (live, not snapshot).
    RoleBound { role: String, pin: String },
    /// Captain-bound fact drained live.
    Fact { summary: String },
    /// 60s ack-SLA beat (30-min SLA needs no faster scan).
    AckSlaTick { now: u64 },
    /// Presence projection row (trusted internal edge).
    Presence { pin: String, state: String },
    /// Inbox rows fetched off-loop re-enter here (pipe pattern).
    InboxFetched {
        pin: String,
        rows: Vec<serde_json::Value>,
    },
    /// Ack-SLA scan results re-enter here (pipe pattern).
    SlaFired { posts: Vec<String> },
    /// Fault-as-data: sensor/sink death, never fatal to the runtime.
    Degraded { source: String, reason: String },
    /// F5: delivery receipt — worker confirms adapter.send succeeded.
    /// Hear/deaf tracking keys off this, not enqueue.
    Delivered { pin: String },
    /// Ask-verb query; reply travels the oneshot (see [`crate::api`]).
    Ask {
        kind: AskKind,
        reply: tokio::sync::oneshot::Sender<AskReply>,
    },
    /// Graceful stop: drain sinks, journal undelivered, exit.
    Shutdown,
}

/// Ask-verb query kinds (read-only fan-out stays on SSE in Phase 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskKind {
    Stats,
    Digest,
}

/// Ask-verb replies.
#[derive(Debug, Clone)]
pub enum AskReply {
    Stats(crate::api::Stats),
    Digest(Vec<(String, String)>),
}


/// GS062-ADV-5: deliver [`BrainMsg::Died`] with `blocking_send` so a Full
/// brain channel cannot drop restart_watch (try_send(Died) is lossy under
/// Full; Watch stays try_send-only to avoid parking the reader on every line).
///
/// Call only from **non-async** threads — `blocking_send` panics inside a
/// tokio runtime (see Host herdr-status path).
pub fn deliver_died_blocking(tx: &tokio::sync::mpsc::Sender<BrainMsg>) {
    let _ = tx.blocking_send(BrainMsg::Died);
}

#[cfg(test)]
mod adv5_tests {
    use super::*;

    /// Under a Full channel, try_send(Died) fails but blocking_send delivers
    /// once capacity frees — restart cannot be silently dropped.
    #[tokio::test(flavor = "current_thread")]
    async fn died_delivered_under_full_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BrainMsg>(1);
        // Fill: next try_send Watch / Died would be Full.
        tx.try_send(BrainMsg::Watch("fill".into())).expect("fill");
        assert!(
            tx.try_send(BrainMsg::Watch("x".into())).is_err(),
            "channel must be Full"
        );
        assert!(
            tx.try_send(BrainMsg::Died).is_err(),
            "try_send(Died) is lossy under Full — the bug under test"
        );

        let tx_b = tx.clone();
        let join = std::thread::spawn(move || {
            // Mirror watch-reader fail path: stop would run first in prod;
            // here we only assert Died delivery.
            deliver_died_blocking(&tx_b);
        });

        // Drain filler so blocking_send can complete.
        match rx.recv().await {
            Some(BrainMsg::Watch(_)) => {}
            other => panic!("expected filler Watch, got {other:?}"),
        }
        match rx.recv().await {
            Some(BrainMsg::Died) => {}
            other => panic!("expected Died under Full, got {other:?}"),
        }
        join.join().expect("deliver thread");
    }

    /// Stamp contract: on_event must not run when Watch enqueue fails.
    #[tokio::test(flavor = "current_thread")]
    async fn on_event_not_stamped_when_watch_send_fails() {
        let dir = std::env::temp_dir().join(format!(
            "dbrt-adv5-stamp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ws = "adv5-ws";
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BrainMsg>(1);
        tx.try_send(BrainMsg::Watch("fill".into())).unwrap();

        let mut stamps = crate::watchbeat::spawn_stamp_threads(dir.clone(), ws.into());
        // Simulate reader: try_send fails → do NOT on_event; stop + Died.
        let line = "watch-line".to_string();
        match tx.try_send(BrainMsg::Watch(line)) {
            Ok(()) => panic!("expected Full"),
            Err(_) => {
                // Correct path: no stamps.on_event() here.
                stamps.stop();
            }
        }
        assert!(
            crate::watchbeat::last_event_ms(&dir, ws).is_none(),
            ".event must stay absent when Watch enqueue failed"
        );
        // Contrast: had we stamped before the failed send, .event would exist.
        // (document the failing-path contract without mutating stamps above.)

        // Died delivery off the runtime thread (blocking_send).
        let tx_b = tx.clone();
        let join = std::thread::spawn(move || deliver_died_blocking(&tx_b));
        let _ = rx.recv().await; // filler
        assert!(matches!(rx.recv().await, Some(BrainMsg::Died)));
        join.join().expect("deliver thread");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

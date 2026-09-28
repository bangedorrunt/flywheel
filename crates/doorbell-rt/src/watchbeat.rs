// governed-by: ADR-0012
//! Watch-delivery stamps (GS-062 honest split):
//! - **Event** file `{state}/doorbell/watch-{workspace}.event` — stamped when a
//!   real NDJSON watch line arrives (`last_event`).
//! - **Heartbeat** file `{state}/doorbell/watch-{workspace}.hb` — stamped on a
//!   periodic 1s tick while the watch child is alive (even with no lines).
//!
//! Never pass `max(event, hb)` into `last_event_ms` (wake_liveness classifier
//! tests encode this). `/health` reads both stamps separately.
//!
//! Why this exists: a watcher pointed at the wrong bus (mailbox env mismatch)
//! or a dead child is *indistinguishable from a quiet workspace* by every other
//! signal. These files are the honest answer to "is the bus delivering to THIS
//! workspace?".

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `{state}/doorbell/watch-{workspace}.hb` (heartbeat only).
pub fn path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("watch-{workspace}.hb"))
}

/// `{state}/doorbell/watch-{workspace}.event` (event lines only).
pub fn event_path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("doorbell")
        .join(format!("watch-{workspace}.event"))
}

/// Stamp a stamp-file with unix-now.
///
/// bd-lmtu: the mkdir is skipped once the parent exists. Now that the ticker is
/// actually wired (it was dead code before), this runs once per second per
/// hosted workspace, so an unconditional `create_dir_all` per stamp is a
/// pointless stat+syscall on the hot path — 11 hosted workspaces is 11 of them
/// a second, forever. Best-effort throughout: a stamp that cannot be written is
/// not an error, it is a missing sample.
fn stamp_file(p: &Path) {
    if let Some(parent) = p.parent()
        && !parent.is_dir()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(p, crate::lease::unix_now().to_string());
}

/// Stamp the **event** file (real watch line arrived). Best-effort.
pub fn touch_event(state_dir: &Path, workspace: &str) {
    stamp_file(&event_path(state_dir, workspace));
}

/// Stamp the **heartbeat** file (periodic tick while watch child alive).
pub fn touch_heartbeat(state_dir: &Path, workspace: &str) {
    stamp_file(&path(state_dir, workspace));
}

/// Back-compat alias: historically conflated event+hb into `.hb`.
/// GS-062: aliases to [`touch_heartbeat`] so callers that meant "pipe alive"
/// stay honest; prefer [`touch_event`] / [`touch_heartbeat`] explicitly.
pub fn touch(state_dir: &Path, workspace: &str) {
    touch_heartbeat(state_dir, workspace);
}

/// Interval for hb ticker and event-stamp throttle. Classifier only needs
/// freshness within deaf_ceiling; 1s matches the pre-GS-062 IO budget.
pub const STAMP_INTERVAL: Duration = Duration::from_secs(1);

/// Spawn a stoppable 1s heartbeat ticker. `send`/`drop` on the returned
/// sender stops it. Stamps `.hb` while the watch subscription/child lives.
///
/// Touches heartbeat on the **calling thread** before `std::thread::spawn` so
/// `/health` cannot observe a NeverStamped window from scheduler delay.
/// Does the ticker keep running after this `recv_timeout` outcome?
///
/// bd-lmtu: extracted as a pure decision because the loop body is exactly where
/// a leak hides, and a thread-level test cannot see it. Treating `Disconnected`
/// as "keep going" does not fail in the obvious direction — it produces a **busy
/// spin**: `recv_timeout` on a dead channel returns `Disconnected` immediately
/// and forever, so the thread spins at full CPU while the stamp file correctly
/// stops changing. A test that only watches the file sees nothing wrong, which
/// is exactly how the first version of this test passed against a mutated
/// build. So the rule is pinned directly, with all three cases.
pub fn ticker_keeps_running(r: &std::sync::mpsc::RecvTimeoutError) -> bool {
    matches!(r, std::sync::mpsc::RecvTimeoutError::Timeout)
}

pub fn spawn_heartbeat_ticker(
    state_dir: PathBuf,
    workspace: String,
) -> std::sync::mpsc::Sender<()> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    // Close NeverStamped on the *calling* thread before spawn — a post-spawn
    // touch still races /health with thread scheduling delay.
    touch_heartbeat(&state_dir, &workspace);
    std::thread::spawn(move || {
        loop {
            match stop_rx.recv_timeout(STAMP_INTERVAL) {
                // An explicit stop.
                Ok(()) => break,
                Err(e) => {
                    if !ticker_keeps_running(&e) {
                        break;
                    }
                    touch_heartbeat(&state_dir, &workspace);
                }
            }
        }
    });
    stop_tx
}

/// Stamp `.event` at most once per [`STAMP_INTERVAL`]. Returns true if written.
pub fn touch_event_throttled(
    state_dir: &Path,
    workspace: &str,
    last: &mut Option<Instant>,
) -> bool {
    if last.is_none_or(|t| t.elapsed() >= STAMP_INTERVAL) {
        touch_event(state_dir, workspace);
        *last = Some(Instant::now());
        true
    } else {
        false
    }
}

/// Shared stoppable 1s hb-ticker + throttled event-stamp for watch paths
/// (file_host child, main `spawn_watch_child`, doorbell_host in-process bus).
///
/// Contract: `.event` on delivered lines/Events (≤1/s); `.hb` every 1s while
/// this handle lives. Drop/`stop` ends the ticker.
pub struct StampThreads {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    state_dir: PathBuf,
    workspace: String,
    last_event: Option<Instant>,
}

impl StampThreads {
    /// Start the hb ticker. Call [`Self::on_event`] on each delivered watch
    /// line / bus Event. Drop or [`Self::stop`] ends the heartbeat.
    ///
    /// Call on the thread that hosts the watch **before** nesting a reader
    /// thread (GS062-ADV-4) so `/health` cannot observe NeverStamped while
    /// the nested reader schedules. [`spawn_heartbeat_ticker`] also touches
    /// `.hb` on this calling thread before its own ticker spawn.
    pub fn spawn(state_dir: PathBuf, workspace: String) -> Self {
        let stop_tx = spawn_heartbeat_ticker(state_dir.clone(), workspace.clone());
        Self {
            stop_tx: Some(stop_tx),
            state_dir,
            workspace,
            last_event: None,
        }
    }

    /// Throttled `.event` stamp (~1s).
    pub fn on_event(&mut self) {
        touch_event_throttled(&self.state_dir, &self.workspace, &mut self.last_event);
    }

    /// Cloneable stop signal — couple to [`Drop`] of a watch child so hb
    /// stops even if the reader thread is parked / slow to exit (GS062-ADV-5).
    pub fn stop_handle(&self) -> Option<std::sync::mpsc::Sender<()>> {
        self.stop_tx.clone()
    }

    /// Stop the heartbeat ticker (also runs on Drop).
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for StampThreads {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start shared stoppable 1s hb-ticker + throttled event-stamp helper.
pub fn spawn_stamp_threads(state_dir: PathBuf, workspace: String) -> StampThreads {
    StampThreads::spawn(state_dir, workspace)
}

/// Absolute mtime of a stamp file in unix **milliseconds**, or `None`.
pub fn stamp_ms(path: &Path) -> Option<u64> {
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(mtime.as_millis() as u64)
}

/// Last event stamp ms (None = never stamped).
pub fn last_event_ms(state_dir: &Path, workspace: &str) -> Option<u64> {
    stamp_ms(&event_path(state_dir, workspace))
}

/// Last heartbeat stamp ms (None = never stamped).
pub fn last_heartbeat_ms(state_dir: &Path, workspace: &str) -> Option<u64> {
    stamp_ms(&path(state_dir, workspace))
}

/// Age of the last **heartbeat** in whole seconds, or `None` when never stamped.
pub fn age_secs(state_dir: &Path, workspace: &str) -> Option<u64> {
    file_age_secs(&path(state_dir, workspace))
}

/// Age of the last **event** in whole seconds, or `None` when never stamped.
pub fn event_age_secs(state_dir: &Path, workspace: &str) -> Option<u64> {
    file_age_secs(&event_path(state_dir, workspace))
}

/// mtime age of an arbitrary stamped file, `None` when missing/unreadable.
pub fn file_age_secs(path: &Path) -> Option<u64> {
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(crate::lease::unix_now().saturating_sub(mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbrt-watchbeat-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn unstamped_is_unknown_not_zero() {
        let d = dir("unknown");
        assert_eq!(age_secs(&d, "w"), None);
        assert_eq!(event_age_secs(&d, "w"), None);
        assert_eq!(last_event_ms(&d, "w"), None);
        assert_eq!(last_heartbeat_ms(&d, "w"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn event_and_hb_are_distinct_stamps() {
        let d = dir("split");
        touch_event(&d, "w");
        assert!(last_event_ms(&d, "w").is_some());
        assert!(
            last_heartbeat_ms(&d, "w").is_none(),
            "event must not stamp hb"
        );
        touch_heartbeat(&d, "w");
        assert!(last_heartbeat_ms(&d, "w").is_some());
        assert_eq!(
            event_path(&d, "w"),
            d.join("doorbell").join("watch-w.event")
        );
        assert_eq!(path(&d, "w"), d.join("doorbell").join("watch-w.hb"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stamp_round_trips_and_is_per_workspace() {
        let d = dir("roundtrip");
        touch_heartbeat(&d, "w");
        assert!(age_secs(&d, "w").is_some_and(|a| a < 5));
        assert_eq!(age_secs(&d, "other"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn event_throttle_skips_within_interval() {
        let d = dir("throttle");
        let mut last = None;
        assert!(touch_event_throttled(&d, "w", &mut last));
        assert!(last_event_ms(&d, "w").is_some());
        // Immediate second call must not rewrite (still Some last).
        assert!(!touch_event_throttled(&d, "w", &mut last));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn spawn_heartbeat_ticker_stamps_immediately() {
        let d = dir("hb-immediate");
        let stop = spawn_heartbeat_ticker(d.clone(), "w".into());
        // Calling-thread touch: stamp must exist before spawn returns (no poll).
        assert!(
            last_heartbeat_ms(&d, "w").is_some(),
            "touch_heartbeat before std::thread::spawn must close NeverStamped gap"
        );
        let _ = stop.send(());
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod bd_lmtu_heartbeat_tests {
    use super::{STAMP_INTERVAL, spawn_heartbeat_ticker};

    /// bd-lmtu: the ticker must ACTUALLY stamp, repeatedly, for as long as it is
    /// held. This is the behaviour that was missing entirely — the function had
    /// no caller, so `.hb` was never a heartbeat and every workspace read as
    /// NeverStamped, which is what leaves wake-liveness unable to tell a QUIET
    /// host from a DEAF one.
    #[test]
    fn the_ticker_stamps_repeatedly_while_held() {
        let dir = std::env::temp_dir().join(format!("hb-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = "wstamp";

        let tx = spawn_heartbeat_ticker(dir.clone(), ws.into());
        // `spawn` touches once on the CALLING thread, so the file must exist
        // before we even wait for a tick.
        let hb = super::path(&dir, ws);
        assert!(hb.exists(), "spawn must stamp synchronously on the caller");

        let first = std::fs::read_to_string(&hb).expect("stamp body");
        std::thread::sleep(STAMP_INTERVAL + std::time::Duration::from_millis(400));
        let second = std::fs::read_to_string(&hb).expect("stamp body");
        assert_ne!(
            first, second,
            "the ticker must keep stamping while held — a single stamp is not a heartbeat"
        );

        drop(tx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The thread must END when the sender drops, or every hosted loop that
    /// restarts leaves a live ticker behind — which is the shape of the thread
    /// leak in bd-lmtu. Prove the exit rather than assuming it: after the drop
    /// the file must stop advancing.
    #[test]
    fn the_ticker_thread_exits_when_the_sender_is_dropped() {
        let dir = std::env::temp_dir().join(format!("hb-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = "wstop";

        let tx = spawn_heartbeat_ticker(dir.clone(), ws.into());
        let hb = super::path(&dir, ws);
        std::thread::sleep(STAMP_INTERVAL + std::time::Duration::from_millis(300));
        assert!(hb.exists(), "stamped while held");

        drop(tx);
        // Give a live ticker time to tick again; a dead one cannot.
        std::thread::sleep(STAMP_INTERVAL + std::time::Duration::from_millis(600));
        let after_drop = std::fs::read_to_string(&hb).expect("stamp body");
        std::thread::sleep(STAMP_INTERVAL + std::time::Duration::from_millis(600));
        let later = std::fs::read_to_string(&hb).expect("stamp body");
        assert_eq!(
            after_drop, later,
            "the ticker kept running after its sender was dropped — that is a thread leak"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact defect, pinned at the source. The ticker was fully implemented
    /// and never called, so nothing failed and nothing looked wrong. A behavioural
    /// test cannot see an uncalled function, so pin the CALL SITE too.
    ///
    /// Anchored on the hosted loop's OWN child spawn, not on the first
    /// `tokio::spawn` in the file. The first one is the reconcile task, so a
    /// window opened there asserts nothing about the loop and passes VACUOUSLY —
    /// which is not hypothetical: this test passed against a build with the
    /// ticker call deleted, and only mutation-checking caught it.
    #[test]
    fn the_hosted_loop_actually_starts_a_ticker() {
        let serve = include_str!("../../doorbell-cli/src/serve.rs");
        let child = serve
            .find("host_workspace(fw, p, w, loop_tx, rx,")
            .expect("hosted loop child spawn");
        let before = &serve[..child];
        let task_start = before
            .rfind("let task = tokio::spawn(async move {")
            .expect("hosted loop task opener");
        let body = &serve[task_start..child];
        // Match the CALL, not the bare identifier. The first version of this
        // assertion used `contains("spawn_heartbeat_ticker")` and PASSED against a
        // build with the call deleted — because the explanatory comment beside the
        // call site names the same function, so the test was matching its own
        // documentation. Pin the qualified call, with its paren.
        assert!(
            body.contains("watchbeat::spawn_heartbeat_ticker("),
            "the hosted loop must CALL watchbeat::spawn_heartbeat_ticker( before its child; \
             without one every workspace stays NeverStamped and wake-liveness cannot classify \
             QUIET vs DEAF"
        );
    }

    /// The loop condition, pinned directly. `Disconnected` must TERMINATE. If it
    /// only continued, the ticker would busy-spin at full CPU on a dead channel
    /// while the stamp file correctly stopped changing — invisible to the
    /// file-watching test above, and worse than the leak it replaced.
    #[test]
    fn only_timeout_keeps_the_ticker_running() {
        use std::sync::mpsc::RecvTimeoutError as E;
        assert!(
            super::ticker_keeps_running(&E::Timeout),
            "a timeout is the tick: stamp and continue"
        );
        assert!(
            !super::ticker_keeps_running(&E::Disconnected),
            "Disconnected is TERMINAL — continuing here busy-spins on a dead channel"
        );
    }

    /// The stamp file must sit where the READER looks, or stamping is invisible.
    /// Pinned because the two halves live in different modules and a future
    /// refactor could move one without the other.
    #[test]
    fn the_stamp_lands_where_the_reader_expects() {
        let dir = std::env::temp_dir().join(format!("hb-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tx = spawn_heartbeat_ticker(dir.clone(), "proj-ws".into());
        let p = super::path(&dir, "proj-ws");
        assert_eq!(
            p,
            dir.join("doorbell").join("watch-proj-ws.hb"),
            "path shape is a contract with the liveness reader"
        );
        assert!(
            p.exists(),
            "the ticker must create the doorbell/ parent on demand"
        );
        drop(tx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! mcc9 (bead `elm-effect-rs-wake-brain-drops-events-mcc9`): the wake-sock →
//! brain hop, with ONE documented policy and exact counters.
//!
//! # Policy (D3: "a slow brain parks sensors")
//!
//! A watch line read off the wake socket is offered to the bounded brain
//! channel ([`crate::BRAIN_CAP`]) by a [`WatchForwarder`], which runs on
//! `spawn_wake_task`'s dedicated std thread — never on a runtime worker, so
//! parking it is legal.
//!
//! 1. `try_send` succeeds → **forwarded**.
//! 2. The channel is full → `wake_brain_full` += 1 and the forwarder PARKS
//!    (backpressure), retrying until capacity frees — with no deadline. A line
//!    that lands after waiting counts as **backpressured** (it is still
//!    forwarded). A slow brain parks its sensor; it never sheds it.
//! 3. Only a CLOSED channel (the brain is gone) ends the consumer: that line
//!    is counted and the caller relays one death. The replacement subscribes
//!    with `since_cursor` = the cursor of the last line the brain ACCEPTED
//!    ([`WatchForwarder::resume_cursor`]).
//!
//! A deadline here was the restart loop: a replay larger than the channel
//! filled it, the first overflow dropped after the window, the consumer was
//! condemned, and the replacement resubscribed at the same cursor and replayed
//! into the same full channel (measured live: 210 drops, one `Died` every
//! ~25s). Backpressure without a cliff lets the cursor advance on every
//! accepted line.
//!
//! Before mcc9 a full channel shed the line immediately, the counter lived in
//! a per-consumer local nobody could read, and the replacement consumer
//! subscribed with NO cursor — replaying the whole retained history into the
//! same 1,024-slot channel, which overflowed again. That loop is what wrote
//! 5,462 `wake_brain_full` rows and 273,296 dropped lines while every digest
//! still said "N queued".
//!
//! # Where the counts are visible
//!
//! * In-process, exact: [`counters_for`] / [`hosted_snapshot`] (the daemon's
//!   `/health` → `wake_brain`, and therefore `doorbell status`).
//! * On disk, survives a restart: [`snapshot_path`] (read by
//!   `flywheel doorbell status` when no daemon answers). Written at sampled
//!   counts and at the end of every full/drop episode, so it can trail the
//!   in-process count only while an episode is still open.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::msg::BrainMsg;

/// Retry spacing while parked. Not a poll of truth: the channel has no
/// timed blocking send usable off-runtime, and this bounds the spin.
const BACKPRESSURE_POLL: Duration = Duration::from_millis(2);

/// How far back a consumer with no persisted cursor subscribes. Whole-retention
/// replay is a second backfill path (and the replay that wedged the brain); the
/// inbox sweep already backfills 24h.
pub const COLD_START_LOOKBACK: Duration = Duration::from_secs(300);

/// The cursor a consumer subscribes from: the last line the brain accepted, or
/// — on a cold start — a bounded lookback from `now`. The event id is a
/// lower-bound filler; the sidecar orders by `(created_at, event_id)`.
#[must_use]
pub fn start_cursor(persisted: Option<String>, now_unix: u64) -> String {
    match persisted {
        Some(c) if !c.is_empty() => c,
        _ => format!(
            "{}:{}",
            now_unix.saturating_sub(COLD_START_LOOKBACK.as_secs()),
            "0".repeat(64)
        ),
    }
}

/// Audit kind for a line that found the brain channel full (sampled rows).
pub const AUDIT_WAKE_BRAIN_FULL: &str = crate::file_host::AUDIT_BRAIN_FULL;
/// Audit kind for a line that never reached the brain (sampled rows).
pub const AUDIT_WAKE_BRAIN_DROPPED: &str = "wake_brain_dropped";
/// Audit kind for a consumer re-armed from a recorded cursor.
pub const AUDIT_WAKE_RESUMED: &str = "wake_resumed_from_cursor";

/// Exact, host-wide (per workspace) counters for the wake → brain hop.
#[derive(Debug, Default)]
pub struct WakeBrainCounters {
    forwarded: AtomicU64,
    wake_brain_full: AtomicU64,
    backpressured: AtomicU64,
    dropped: AtomicU64,
    resumed: AtomicU64,
    /// toron#17 `events_lost` frames seen (cap eviction upstream of us).
    events_lost: AtomicU64,
    /// Unix seconds of the newest drop; 0 = never.
    last_drop_unix: AtomicU64,
    /// A full/drop happened that the on-disk snapshot does not show yet.
    dirty: AtomicBool,
}

/// A point-in-time read of [`WakeBrainCounters`] (the `/health` shape).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WakeBrainSnapshot {
    /// Lines that reached the brain (including backpressured ones).
    pub forwarded: u64,
    /// Lines that found the brain channel full on first offer.
    pub wake_brain_full: u64,
    /// Lines that waited for room and then landed (not lost).
    pub backpressured: u64,
    /// Lines that never reached the brain (replayed by the next consumer from
    /// its resume cursor).
    pub dropped: u64,
    /// Consumers re-armed from a recorded cursor.
    pub resumed: u64,
    /// `events_lost` frames from the wake sidecar (toron#17): stored events
    /// after our cursor were evicted before we read them. Each frame is one
    /// counted gap; the range is in the audit row, verbatim.
    #[serde(default)]
    pub events_lost: u64,
    /// Unix seconds of the newest drop; `None` = never.
    pub last_drop_unix: Option<u64>,
}

impl WakeBrainSnapshot {
    /// Sum two snapshots (the `/health` totals row).
    #[must_use]
    pub fn plus(self, o: Self) -> Self {
        Self {
            forwarded: self.forwarded + o.forwarded,
            wake_brain_full: self.wake_brain_full + o.wake_brain_full,
            backpressured: self.backpressured + o.backpressured,
            dropped: self.dropped + o.dropped,
            resumed: self.resumed + o.resumed,
            events_lost: self.events_lost + o.events_lost,
            last_drop_unix: self.last_drop_unix.max(o.last_drop_unix),
        }
    }
}

impl WakeBrainCounters {
    #[must_use]
    pub fn snapshot(&self) -> WakeBrainSnapshot {
        let last = self.last_drop_unix.load(Ordering::SeqCst);
        WakeBrainSnapshot {
            forwarded: self.forwarded.load(Ordering::SeqCst),
            wake_brain_full: self.wake_brain_full.load(Ordering::SeqCst),
            backpressured: self.backpressured.load(Ordering::SeqCst),
            dropped: self.dropped.load(Ordering::SeqCst),
            resumed: self.resumed.load(Ordering::SeqCst),
            events_lost: self.events_lost.load(Ordering::SeqCst),
            last_drop_unix: (last > 0).then_some(last),
        }
    }

    /// A consumer was re-armed from a recorded cursor.
    pub fn note_resume(&self) {
        self.resumed.fetch_add(1, Ordering::SeqCst);
    }
}

fn registry() -> &'static Mutex<std::collections::BTreeMap<String, Arc<WakeBrainCounters>>> {
    static REG: OnceLock<Mutex<std::collections::BTreeMap<String, Arc<WakeBrainCounters>>>> =
        OnceLock::new();
    REG.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

/// The process-wide counters for one workspace (created on first ask). Every
/// consumer a host re-arms shares them, so a restart never resets the count.
#[must_use]
pub fn counters_for(workspace: &str) -> Arc<WakeBrainCounters> {
    let mut g = registry().lock().unwrap_or_else(|e| e.into_inner());
    g.entry(workspace.to_string()).or_default().clone()
}

/// The exact in-process snapshot for a workspace this process hosts, or
/// `None` when nothing here ever forwarded for it.
#[must_use]
pub fn hosted_snapshot(workspace: &str) -> Option<WakeBrainSnapshot> {
    let g = registry().lock().unwrap_or_else(|e| e.into_inner());
    g.get(workspace).map(|c| c.snapshot())
}

/// On-disk snapshot: `<flywheel_state>/doorbell/<workspace>.wake-brain.json`.
#[must_use]
pub fn snapshot_path(flywheel_state: &Path, workspace: &str) -> PathBuf {
    flywheel_state
        .join("doorbell")
        .join(format!("{workspace}.wake-brain.json"))
}

/// Read the on-disk snapshot; `None` when absent or unreadable.
#[must_use]
pub fn read_snapshot(flywheel_state: &Path, workspace: &str) -> Option<WakeBrainSnapshot> {
    let raw = std::fs::read(snapshot_path(flywheel_state, workspace)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// The last line the brain ACCEPTED, persisted across consumer restarts.
///
/// Without it a replacement consumer resumed from origin: the sidecar sent its
/// whole retention (measured 26h), the brain channel filled, lines were
/// dropped, and the replacement resumed from the same origin again.
#[must_use]
pub fn cursor_path(flywheel_state: &Path, workspace: &str) -> PathBuf {
    flywheel_state
        .join("doorbell")
        .join(format!("{workspace}.wake-cursor"))
}

pub fn write_cursor(flywheel_state: &Path, workspace: &str, cursor: &str) {
    let path = cursor_path(flywheel_state, workspace);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, cursor);
}

#[must_use]
pub fn read_cursor(flywheel_state: &Path, workspace: &str) -> Option<String> {
    let raw = std::fs::read_to_string(cursor_path(flywheel_state, workspace)).ok()?;
    let cursor = raw.trim();
    (!cursor.is_empty()).then(|| cursor.to_string())
}

/// Write the snapshot atomically (tmp + rename, 0600 like the audit rows).
pub fn write_snapshot(path: &Path, snap: &WakeBrainSnapshot) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_vec(snap).map_err(std::io::Error::other)?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
}

/// The sidecar cursor (`<created_at>:<event_id>`) of one watch line, or `None`
/// when the line carries no 64-hex id. The sidecar refuses a malformed cursor
/// outright, so no cursor (replay from origin) beats a wrong one.
#[must_use]
pub fn watch_line_cursor(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Fields {
        id: String,
        created_at: u64,
    }
    let f: Fields = serde_json::from_str(line).ok()?;
    (f.id.len() == 64 && f.id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("{}:{}", f.created_at, f.id.to_ascii_lowercase()))
}

/// Clock + park port, so the interpreter test drives the wait with a fake
/// clock instead of real sleep.
pub trait Park: Send + Sync {
    fn now(&self) -> Instant;
    fn park(&self, d: Duration);
}

/// Production park: the monotonic clock and a thread sleep.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThreadPark;

impl Park for ThreadPark {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn park(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// What one offered line did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarded {
    /// Landed on first offer.
    Delivered,
    /// Found the channel full, waited, landed. Not lost.
    Backpressured,
    /// Never reached the brain. `report_death` is true for exactly one drop per
    /// consumer (the caller relays one `Died` so the consumer is replaced);
    /// `closed` names a brain that is gone rather than wedged.
    Dropped { report_death: bool, closed: bool },
}

impl Forwarded {
    /// Did the brain receive the line?
    #[must_use]
    pub fn reached_brain(self) -> bool {
        matches!(self, Self::Delivered | Self::Backpressured)
    }
}

type AuditFn = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// One consumer's forwarder (see the module policy). Cheap to build; build a
/// fresh one per consumer and share `counters` + `resume` across them.
pub struct WatchForwarder {
    tx: tokio::sync::mpsc::Sender<BrainMsg>,
    counters: Arc<WakeBrainCounters>,
    resume: Arc<Mutex<Option<String>>>,
    condemned: AtomicBool,
    park: Arc<dyn Park>,
    audit: Option<AuditFn>,
    persist_to: Option<PathBuf>,
    unix_now: fn() -> u64,
}

impl WatchForwarder {
    #[must_use]
    pub fn new(
        tx: tokio::sync::mpsc::Sender<BrainMsg>,
        counters: Arc<WakeBrainCounters>,
        resume: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            tx,
            counters,
            resume,
            condemned: AtomicBool::new(false),
            park: Arc::new(ThreadPark),
            audit: None,
            persist_to: None,
            unix_now: crate::lease::unix_now,
        }
    }

    /// Substitute the clock/park port (interpreter tests).
    #[must_use]
    pub fn with_park(mut self, park: Arc<dyn Park>) -> Self {
        self.park = park;
        self
    }

    /// Sampled audit rows (first, then every power of two) for full and drop.
    #[must_use]
    pub fn with_audit(mut self, audit: AuditFn) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Persist the snapshot to this path at sampled counts and episode ends.
    #[must_use]
    pub fn persist_to(mut self, path: PathBuf) -> Self {
        self.persist_to = Some(path);
        self
    }

    /// The cursor the next consumer must subscribe from: the last line the
    /// brain accepted. `None` until one landed.
    #[must_use]
    pub fn resume_cursor(&self) -> Option<String> {
        self.resume.lock().map(|g| g.clone()).unwrap_or(None)
    }

    /// toron#17: the sidecar announced a gap. Count it exactly and persist
    /// at once (a gap is rare, and a snapshot that lags it hides a loss).
    /// The audit row naming the range is written by the wake consumer.
    pub fn note_events_lost(&self) {
        self.counters.events_lost.fetch_add(1, Ordering::SeqCst);
        self.counters
            .last_drop_unix
            .store((self.unix_now)(), Ordering::SeqCst);
        self.counters.dirty.store(true, Ordering::SeqCst);
        self.persist();
    }

    /// Offer one watch line to the brain under the module policy.
    pub fn forward(&self, line: String) -> Forwarded {
        if self.condemned.load(Ordering::SeqCst) {
            return self.drop_line(false, false);
        }
        let cursor = watch_line_cursor(&line);
        let mut msg = BrainMsg::Watch(line);
        let mut parked = false;
        loop {
            match self.tx.try_send(msg) {
                Ok(()) if parked => {
                    self.counters.backpressured.fetch_add(1, Ordering::SeqCst);
                    return self.landed(cursor, Forwarded::Backpressured);
                }
                Ok(()) => return self.landed(cursor, Forwarded::Delivered),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return self.condemn_and_drop(true);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(m)) => msg = m,
            }
            if !parked {
                let prior = self.counters.wake_brain_full.fetch_add(1, Ordering::SeqCst);
                self.counters.dirty.store(true, Ordering::SeqCst);
                if crate::file_host::should_audit_count(prior as usize) {
                    self.audit_row(
                        AUDIT_WAKE_BRAIN_FULL,
                        "brain channel full; forwarder parked (backpressure), line not shed",
                    );
                    self.persist();
                }
                parked = true;
            }
            self.park.park(BACKPRESSURE_POLL);
        }
    }

    fn landed(&self, cursor: Option<String>, how: Forwarded) -> Forwarded {
        self.counters.forwarded.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = cursor
            && let Ok(mut g) = self.resume.lock()
        {
            *g = Some(c);
        }
        if self.counters.dirty.swap(false, Ordering::SeqCst) {
            self.persist();
        }
        how
    }

    fn condemn_and_drop(&self, closed: bool) -> Forwarded {
        let first = !self.condemned.swap(true, Ordering::SeqCst);
        self.drop_line(first, closed)
    }

    fn drop_line(&self, report_death: bool, closed: bool) -> Forwarded {
        let prior = self.counters.dropped.fetch_add(1, Ordering::SeqCst);
        self.counters
            .last_drop_unix
            .store((self.unix_now)(), Ordering::SeqCst);
        self.counters.dirty.store(true, Ordering::SeqCst);
        if report_death || crate::file_host::should_audit_count(prior as usize) {
            self.audit_row(
                AUDIT_WAKE_BRAIN_DROPPED,
                if closed {
                    "brain channel closed; line not enqueued, next consumer resumes from the cursor"
                } else {
                    "consumer already condemned; line not enqueued, the replacement resumes from the cursor"
                },
            );
            self.persist();
        }
        Forwarded::Dropped {
            report_death,
            closed,
        }
    }

    fn audit_row(&self, kind: &str, reason: &str) {
        let Some(audit) = &self.audit else { return };
        let s = self.counters.snapshot();
        audit(
            kind,
            serde_json::json!({
                "reason": reason,
                "full_total": s.wake_brain_full,
                "backpressured_total": s.backpressured,
                "dropped_total": s.dropped,
                "forwarded_total": s.forwarded,
                "resume_from": self.resume_cursor(),
            }),
        );
    }

    fn persist(&self) {
        if let Some(path) = &self.persist_to
            && let Err(e) = write_snapshot(path, &self.counters.snapshot())
        {
            eprintln!(
                "doorbell: wake-brain snapshot write failed ({}): {e}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// The resume cursor survives a restart: an absent file reads as `None`,
    /// and a written cursor reads back trimmed so the next consumer resumes at
    /// the line the brain accepted instead of the sidecar's origin.
    #[test]
    fn cursor_round_trips_and_absent_reads_none() {
        let dir = std::env::temp_dir().join(format!("wb-cursor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_cursor(&dir, "ws"), None);
        write_cursor(&dir, "ws", "1099:abc\n");
        assert_eq!(read_cursor(&dir, "ws").as_deref(), Some("1099:abc"));
        write_cursor(&dir, "ws", "");
        assert_eq!(read_cursor(&dir, "ws"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// toron#17: every `events_lost` frame is counted exactly, persisted at
    /// once, and degrades health; a quiet run counts zero (negative control).
    #[test]
    fn events_lost_frames_are_counted_exactly_and_persisted() {
        let dir = std::env::temp_dir().join(format!("wb-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("ws.wake-brain.json");
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let fw = WatchForwarder::new(tx, fresh_counters(), Arc::new(Mutex::new(None)))
            .persist_to(path.clone());
        assert_eq!(
            fw.counters.snapshot().events_lost,
            0,
            "none seen, none counted"
        );
        for _ in 0..3 {
            fw.note_events_lost();
        }
        let s = fw.counters.snapshot();
        assert_eq!(s.events_lost, 3, "exact, one per frame");
        assert!(s.last_drop_unix.is_some(), "a gap degrades health");
        let on_disk = read_snapshot(&dir, "ws");
        assert_eq!(on_disk.map(|d| d.events_lost), None, "path shape differs");
        let raw: WakeBrainSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw.events_lost, 3, "persisted at once");
        let old: WakeBrainSnapshot = serde_json::from_str(
            r#"{"forwarded":1,"wake_brain_full":0,"backpressured":0,"dropped":0,"resumed":0,"last_drop_unix":null}"#,
        )
        .unwrap();
        assert_eq!(old.events_lost, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hex(n: u64) -> String {
        format!("{n:064x}")
    }

    fn line(n: u64) -> String {
        crate::wake_sock::format_watch_line(9, &hex(n), "pk", "c", &[], 1_000 + n)
    }

    fn fresh_counters() -> Arc<WakeBrainCounters> {
        Arc::new(WakeBrainCounters::default())
    }

    /// Fake clock: every park advances time by the park duration; an optional
    /// hook runs on each park (the "brain" making room, or not).
    struct FakePark {
        t0: Instant,
        elapsed: Mutex<Duration>,
        parks: AtomicUsize,
        on_park: Box<dyn Fn(usize) + Send + Sync>,
    }

    impl FakePark {
        fn new(on_park: impl Fn(usize) + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                t0: Instant::now(),
                elapsed: Mutex::new(Duration::ZERO),
                parks: AtomicUsize::new(0),
                on_park: Box::new(on_park),
            })
        }
    }

    impl Park for FakePark {
        fn now(&self) -> Instant {
            self.t0 + *self.elapsed.lock().unwrap()
        }
        fn park(&self, d: Duration) {
            *self.elapsed.lock().unwrap() += d;
            let n = self.parks.fetch_add(1, Ordering::SeqCst);
            (self.on_park)(n);
        }
    }

    /// A cold start asks for a bounded lookback, never the sidecar's whole
    /// retention; a persisted cursor wins unchanged. The zero-filled event id
    /// is a lower bound in the sidecar's `(created_at, event_id)` order.
    #[test]
    fn a_cold_start_cursor_is_a_bounded_lookback_not_origin() {
        let cold = start_cursor(None, 10_000);
        assert_eq!(
            cold,
            format!(
                "{}:{}",
                10_000 - COLD_START_LOOKBACK.as_secs(),
                "0".repeat(64)
            )
        );
        assert_ne!(cold, "head");
        assert_eq!(start_cursor(Some(String::new()), 10_000), cold);
        let warm = format!("1007:{}", hex(7));
        assert_eq!(start_cursor(Some(warm.clone()), 10_000), warm);
    }

    #[test]
    fn watch_line_cursor_is_the_sidecar_form_or_none() {
        assert_eq!(
            watch_line_cursor(&line(7)).as_deref(),
            Some(format!("1007:{}", hex(7)).as_str())
        );
        let short = crate::wake_sock::format_watch_line(9, "abc", "pk", "c", &[], 5);
        assert_eq!(watch_line_cursor(&short), None);
        assert_eq!(watch_line_cursor("not json"), None);
    }

    /// Zero-drop negative control: under normal load (the brain keeps up)
    /// every line is Delivered and every drop/full counter stays 0.
    #[test]
    fn normal_load_counts_zero_drops() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let c = fresh_counters();
        let fw = WatchForwarder::new(tx, c.clone(), Arc::new(Mutex::new(None)));
        for n in 0..100 {
            assert_eq!(fw.forward(line(n)), Forwarded::Delivered);
            rx.try_recv().expect("brain keeps up");
        }
        let s = c.snapshot();
        assert_eq!(
            (s.forwarded, s.wake_brain_full, s.backpressured, s.dropped),
            (100, 0, 0, 0)
        );
        assert_eq!(s.last_drop_unix, None);
        assert_eq!(fw.resume_cursor(), Some(format!("1099:{}", hex(99))));
    }

    /// Interpreter test, backpressure arm: a full channel parks the forwarder
    /// and the line lands once the (fake) brain makes room — counted as
    /// backpressured, never dropped. No real sleep.
    #[test]
    fn a_slow_brain_backpressures_instead_of_shedding() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(BrainMsg::Watch("filler".into())).unwrap();
        let rx = Arc::new(Mutex::new(rx));
        let brain = rx.clone();
        let park = FakePark::new(move |n| {
            if n == 2 {
                brain.lock().unwrap().try_recv().unwrap();
            }
        });
        let c = fresh_counters();
        let fw = WatchForwarder::new(tx, c.clone(), Arc::new(Mutex::new(None))).with_park(park);
        assert_eq!(fw.forward(line(1)), Forwarded::Backpressured);
        let s = c.snapshot();
        assert_eq!(
            (s.forwarded, s.wake_brain_full, s.backpressured, s.dropped),
            (1, 1, 1, 0)
        );
        match rx.lock().unwrap().try_recv() {
            Ok(BrainMsg::Watch(l)) => assert_eq!(l, line(1), "the parked line itself landed"),
            other => panic!("expected the backpressured line, got {other:?}"),
        }
    }

    /// A replay larger than the channel neither drops nor ends the consumer:
    /// every line parks until the brain takes it, and the cursor ends past the
    /// last event. This is the shape that looped live (a 1,321-line replay into
    /// a 1,024-slot channel): the old deadline dropped and condemned here.
    #[test]
    fn a_replay_larger_than_the_channel_neither_drops_nor_ends_the_consumer() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let rx = Arc::new(Mutex::new(rx));
        let brain = rx.clone();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = seen.clone();
        let park = FakePark::new(move |_| {
            if let Ok(BrainMsg::Watch(l)) = brain.lock().unwrap().try_recv() {
                sink.lock().unwrap().push(l);
            }
        });
        let c = fresh_counters();
        let resume = Arc::new(Mutex::new(None));
        let fw = WatchForwarder::new(tx, c.clone(), resume.clone()).with_park(park.clone());
        let outcomes: Vec<Forwarded> = (1..=50).map(|n| fw.forward(line(n))).collect();
        assert!(
            outcomes.iter().all(|o| o.reached_brain()),
            "a slow brain must never shed a line: {outcomes:?}"
        );
        let s = c.snapshot();
        assert_eq!(s.forwarded, 50);
        assert_eq!(s.dropped, 0);
        assert_eq!(s.backpressured, 46, "every line past the 4 slots parked");
        assert_eq!(s.wake_brain_full, 46);
        assert!(park.parks.load(Ordering::SeqCst) >= 46);
        assert_eq!(
            resume.lock().unwrap().clone(),
            Some(format!("1050:{}", hex(50))),
            "the cursor ends past the last accepted event"
        );
        while let Ok(BrainMsg::Watch(l)) = rx.lock().unwrap().try_recv() {
            seen.lock().unwrap().push(l);
        }
        let got = seen.lock().unwrap().clone();
        let want: Vec<String> = (1..=50).map(line).collect();
        assert_eq!(got, want, "order preserved, nothing lost");
    }

    /// A closed channel (brain gone) drops without waiting and is flagged.
    #[test]
    fn a_closed_brain_drops_at_once_and_says_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        drop(rx);
        let park = FakePark::new(|_| {});
        let c = fresh_counters();
        let fw =
            WatchForwarder::new(tx, c.clone(), Arc::new(Mutex::new(None))).with_park(park.clone());
        assert_eq!(
            fw.forward(line(1)),
            Forwarded::Dropped {
                report_death: true,
                closed: true
            }
        );
        assert_eq!(park.parks.load(Ordering::SeqCst), 0);
        assert_eq!(c.snapshot().dropped, 1);
    }

    /// Counters are host-wide: a replacement forwarder keeps counting, and the
    /// disk snapshot catches up at the end of the episode.
    #[test]
    fn counters_survive_consumer_replacement_and_persist_at_episode_end() {
        let dir = std::env::temp_dir().join(format!(
            "wake-brain-persist-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        let path = snapshot_path(&dir, "ws-persist");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let c = fresh_counters();
        let resume = Arc::new(Mutex::new(None));
        let first = WatchForwarder::new(tx.clone(), c.clone(), resume.clone())
            .with_park(FakePark::new(|_| {}))
            .persist_to(path.clone());
        assert_eq!(first.forward(line(1)), Forwarded::Delivered);
        drop(rx);
        assert!(matches!(
            first.forward(line(2)),
            Forwarded::Dropped {
                report_death: true,
                closed: true
            }
        ));
        assert_eq!(c.snapshot().dropped, 1);
        let (tx2, mut rx2) = tokio::sync::mpsc::channel(1);
        let second = WatchForwarder::new(tx2, c.clone(), resume)
            .with_park(FakePark::new(|_| {}))
            .persist_to(path.clone());
        assert_eq!(second.resume_cursor(), Some(format!("1001:{}", hex(1))));
        assert_eq!(second.forward(line(2)), Forwarded::Delivered);
        rx2.try_recv().unwrap();
        let on_disk: WakeBrainSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk, c.snapshot(), "episode end flushed the exact count");
        assert_eq!((on_disk.dropped, on_disk.forwarded), (1, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_hands_out_one_counter_per_workspace() {
        let a = counters_for("ws-registry-unique-a");
        let b = counters_for("ws-registry-unique-a");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(hosted_snapshot("ws-registry-never-hosted").is_none());
        assert_eq!(
            hosted_snapshot("ws-registry-unique-a"),
            Some(WakeBrainSnapshot::default())
        );
    }

    fn uuid_like() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

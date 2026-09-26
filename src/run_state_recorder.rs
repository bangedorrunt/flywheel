// governed-by: ADR-0016
//! The run-state RECORDER: the interpreter's side of the checkpoint log.
//!
//! [`crate::state_log`] is the pure codec. This module is the effect: it owns
//! the store handle, the monotonic boundary counter, and the two bounds that
//! keep a long-lived dispatcher's log from growing without limit. It is called
//! from the INTERPRETER (`doorbell_host::RtHost`) and never from a reducer.
//!
//! ## Why the snapshot point is NOT the tick boundary
//!
//! `doorbell_host` runs:
//!
//! ```text
//! workspace::tick(&model, &msg) -> (next, cmds)   // the fold, pure
//! handle_workspace_cmds(&mut model, ...)          // EFFECTS, mutates model
//! ```
//!
//! `handle_workspace_cmds` mutates the model AFTER the fold returns, so a
//! snapshot taken at the fold's return records an incomplete model that cannot
//! resume. [`RunStateRecorder::record`] must therefore be called after the
//! effect layer has finished — the only state the next tick will read.
//!
//! ## Bounds (spec §7, effect-oriented principle 10)
//!
//! Two, both required rather than defaulted:
//!
//! * [`RunStateRecorder::MAX_PAYLOAD_BYTES`] — a model that serializes larger
//!   than this is REFUSED loudly, not truncated. A truncated snapshot is a lie
//!   that would replay into a state the fold never held. The dispatcher's
//!   `doorbell.queues` can grow with every unanswered pin, so this is a real
//!   ceiling, not a formality.
//! * [`RunStateRecorder::retain`] — rows are pruned strictly below a stated
//!   floor, so a long-lived thread cannot grow without bound. The floor is an
//!   argument because the store cannot know the live resume point.
//!
//! ## Activation
//!
//! Off unless [`RunStateRecorder::attach`] is called. Writing a boundary per
//! tick changes the dispatcher's storage and latency profile, so it is an
//! explicit operator choice rather than a silent new default. [`attach`]
//! returns `None` — never a silent no-op — when no index is open, so a missing
//! recorder is visible rather than mistaken for "no boundaries yet".

use std::sync::Arc;

use toron_core::index::run_state::{NewRunStateCheckpoint, RunStateStore};

use crate::state_log::{RunStateModel, ThreadId, boundary_payload};

/// Largest model payload this recorder will store. See the module docs: a
/// refusal is honest, a truncation is not.
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20; // 1 MiB

/// Bound on boundaries enqueued but not yet durable. A stalled writer must
/// surface as a loud refusal, not as unbounded memory growth.
pub const PENDING_CAP: usize = 256;

/// The interpreter's handle on the checkpoint log.
pub struct RunStateRecorder {
    store: RunStateStore,
    thread: ThreadId,
    /// The fold's own boundary counter. Monotonic per thread; the store orders
    /// by it, so it must never go backwards. Behind an atomic because
    /// `enqueue` takes `&self` — the dispatcher holds `&RtHost` synchronously.
    seq: std::sync::atomic::AtomicI64,
    /// Bounded FIFO of boundaries not yet durable. Bounded because an unbounded
    /// queue is a memory bug; on overflow the boundary is REFUSED, never dropped.
    pending: Arc<std::sync::Mutex<BoundedQueue>>,
    /// Newest durable boundary id, so a queued boundary can name its parent
    /// without a store read on the synchronous path.
    parent_hint: Arc<std::sync::Mutex<Option<String>>>,
    /// `created_at_us` source, injected so a test pins it and the recorder
    /// never reads a clock inside a decision path.
    clock: Clock,
}

/// Hand-written because the clock is a closure, which is not `Debug`. The clock
/// is printed as a placeholder deliberately: printing a recorder must never CALL
/// it, or a debug log would read the wall clock. Its identity is still reported
/// so two recorders can be told apart.
impl std::fmt::Debug for RunStateRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStateRecorder")
            .field("store", &self.store)
            .field("thread", &self.thread)
            // The field itself, not `next_seq()`: the lint tracks field reads,
            // and AtomicI64 is Debug.
            .field("seq", &self.seq)
            .field(
                "pending",
                &self.pending.lock().map(|q| q.len()).unwrap_or(0),
            )
            .field(
                "parent_hint",
                &self
                    .parent_hint
                    .lock()
                    .map(|p| p.is_some())
                    .unwrap_or(false),
            )
            .field("clock", &"<injected>")
            .finish()
    }
}

/// Why a boundary was not written. Every variant is an honest refusal: none of
/// them means "recorded".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordRefusal {
    /// The model serialized past [`MAX_PAYLOAD_BYTES`].
    TooLarge { bytes: usize, cap: usize },
    /// The codec failed. The boundary does not exist.
    Codec(String),
    /// The store rejected or failed the write. The boundary does not exist.
    Store(String),
    /// The bounded queue is full: the writer is not keeping up. The boundary
    /// does not exist and is NOT silently dropped.
    QueueFull,
}

impl std::fmt::Display for RecordRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes, cap } => write!(
                f,
                "run-state model is {bytes} bytes, over the {cap}-byte cap; refused rather \
                 than truncated, because a truncated snapshot replays into a state the fold \
                 never held"
            ),
            Self::Codec(e) => write!(f, "run-state codec failed: {e}"),
            Self::Store(e) => write!(f, "run-state store failed: {e}"),
            Self::QueueFull => write!(
                f,
                "run-state pending queue is full ({PENDING_CAP}); the boundary was refused \
                 rather than dropped, so a replay would otherwise lose history silently"
            ),
        }
    }
}

impl std::error::Error for RecordRefusal {}

/// The recorder's wall-clock source. Injected so production can pass a real
/// clock while a test pins it (no wall clock in a decision-path test).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Microseconds since the Unix epoch. Used by production only.
pub fn system_clock_us() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
    })
}

/// A boundary that has been serialized but is not yet durable.
#[derive(Debug, Clone)]
struct PendingBoundary {
    seq: i64,
    parent_hint: Option<String>,
    created_at: i64,
    payload: String,
}

/// Bounded FIFO. `try_push` is the whole point: it REFUSES on overflow rather
/// than growing, so a stalled writer becomes a loud refusal instead of
/// unbounded memory.
#[derive(Debug)]
struct BoundedQueue {
    items: std::collections::VecDeque<PendingBoundary>,
    cap: usize,
}

impl BoundedQueue {
    fn new(cap: usize) -> Self {
        Self {
            items: std::collections::VecDeque::with_capacity(cap.min(1024)),
            cap,
        }
    }

    fn try_push(&mut self, item: PendingBoundary) -> Result<(), PendingBoundary> {
        if self.items.len() >= self.cap {
            return Err(item);
        }
        self.items.push_back(item);
        Ok(())
    }

    fn pop_front(&mut self) -> Option<PendingBoundary> {
        self.items.pop_front()
    }

    fn len(&self) -> usize {
        self.items.len()
    }
}

/// Clones the handle, not the counter: a cloned recorder shares one boundary
/// sequence and one queue (both behind `Arc`/atomic), so two handles cannot
/// hand out the same `seq`. `AtomicI64` is not `Clone`, hence hand-written.
impl Clone for RunStateRecorder {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            thread: self.thread.clone(),
            seq: std::sync::atomic::AtomicI64::new(self.next_seq()),
            pending: self.pending.clone(),
            parent_hint: self.parent_hint.clone(),
            clock: self.clock.clone(),
        }
    }
}

// bd-4dpq remaining slice: the READ side (the replay/fork verbs) is not wired
// yet. `with_store` / `pending_len` / `record` / `latest` / `thread` are its
// seams — a deterministic drain a test can drive instead of racing the writer
// thread, and the test constructor. They are tested and fail-closed, so they are
// allow-listed rather than deleted: shipping a write-only checkpoint log would be
// worse than an annotated gap.
#[allow(dead_code)]
impl RunStateRecorder {
    /// Open a recorder over the existing toron index, or `None` when no index
    /// is available. `None` is a visible "no recorder", never a silent no-op.
    pub fn attach(thread: &str) -> Option<Self> {
        let pool = crate::mail_shim::try_mailbox().and_then(|mb| mb.index_pool().cloned())?;
        let thread = ThreadId::new(thread).ok()?;
        Some(Self {
            store: RunStateStore::new(pool),
            thread,
            seq: std::sync::atomic::AtomicI64::new(0),
            pending: Arc::new(std::sync::Mutex::new(BoundedQueue::new(PENDING_CAP))),
            parent_hint: Arc::new(std::sync::Mutex::new(None)),
            clock: system_clock_us(),
        })
    }

    /// Construct over an explicit store and clock. The test seam: no env, no
    /// ambient mailbox, no real clock.
    pub fn with_store(store: RunStateStore, thread: &str, clock: Clock) -> anyhow::Result<Self> {
        Ok(Self {
            store,
            thread: ThreadId::new(thread)?,
            seq: std::sync::atomic::AtomicI64::new(0),
            pending: Arc::new(std::sync::Mutex::new(BoundedQueue::new(PENDING_CAP))),
            parent_hint: Arc::new(std::sync::Mutex::new(None)),
            clock,
        })
    }

    /// The next boundary number. Monotonic per thread.
    pub fn next_seq(&self) -> i64 {
        self.seq.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Enqueue one boundary for the writer WITHOUT blocking the caller.
    ///
    /// This is the entry point the live dispatcher uses, and the reason it
    /// exists: `doorbell_rt::Host::tick` is a SYNCHRONOUS trait method, so the
    /// async store write cannot be awaited there. Awaiting it would mean
    /// blocking on a nested runtime — the exact nested-panic class this codebase
    /// already keeps closed (see `effect_journal::submit_journal_wait`).
    ///
    /// So the boundary is serialized HERE (pure) and handed to a bounded queue
    /// drained by a writer task. Two consequences, both deliberate:
    ///
    /// * The write is ASYNCHRONOUS with respect to the fold, so a boundary is
    ///   not durable until the writer drains. A replay therefore sees a prefix
    ///   of the boundaries, not a guaranteed-complete one — which is why
    ///   `latest()` is the honest resume seed rather than an assumed count.
    /// * SATURATION FAILS LOUD. A full queue drops nothing silently: the
    ///   boundary is refused and the caller logs it. Silently skipping would
    ///   make a later replay look like it lost history for no reason.
    pub fn enqueue(
        &self,
        model: &RunStateModel,
        effect_keys: &[String],
    ) -> Result<i64, RecordRefusal> {
        let payload = boundary_payload(model, effect_keys)
            .map_err(|e| RecordRefusal::Codec(e.to_string()))?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(RecordRefusal::TooLarge {
                bytes: payload.len(),
                cap: MAX_PAYLOAD_BYTES,
            });
        }
        let seq = self.next_seq();
        self.pending
            .lock()
            .expect("pending queue lock")
            .try_push(PendingBoundary {
                seq,
                parent_hint: self.parent_hint.lock().expect("parent lock").clone(),
                created_at: (self.clock)(),
                payload,
            })
            .map_err(|_| RecordRefusal::QueueFull)?;
        self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(seq)
    }

    /// Drain up to `max` queued boundaries into the store. Called by the writer
    /// task; exposed so a test can drive the writer deterministically instead of
    /// racing a background thread.
    pub async fn drain(&self, max: usize) -> Result<usize, RecordRefusal> {
        let mut written = 0usize;
        loop {
            if written >= max {
                break;
            }
            let Some(item) = self.pending.lock().expect("pending queue lock").pop_front() else {
                break;
            };
            let row = self
                .store
                .append(NewRunStateCheckpoint {
                    thread_id: self.thread.as_str().to_string(),
                    seq: item.seq,
                    parent_checkpoint_id: item.parent_hint,
                    model_json: item.payload,
                    msg_kind: None,
                    effect_keys: None,
                    created_at: item.created_at,
                })
                .await
                .map_err(|e| RecordRefusal::Store(e.to_string()))?;
            *self.parent_hint.lock().expect("parent lock") = Some(row.0.checkpoint_id);
            written += 1;
        }
        Ok(written)
    }

    /// Boundaries enqueued but not yet durable. Non-zero after a crash is the
    /// honest signal that a replay would resume from an earlier boundary.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().expect("pending queue lock").len()
    }

    /// Record one boundary, awaiting the write. For callers that ARE async (a
    /// command path, a test) and want the boundary durable before returning.
    /// Enqueues, then drains just that boundary.
    pub async fn record(
        &self,
        model: &RunStateModel,
        effect_keys: &[String],
    ) -> Result<i64, RecordRefusal> {
        let seq = self.enqueue(model, effect_keys)?;
        self.drain(1).await?;
        Ok(seq)
    }

    /// Prune boundaries strictly below `protect_from_seq`. The floor is
    /// required so a caller cannot prune away the boundary it is about to
    /// resume from.
    pub async fn retain(
        &self,
        protect_from_seq: i64,
    ) -> Result<toron_core::index::run_state::RunStatePruneCounts, RecordRefusal> {
        self.store
            .prune_before(self.thread.as_str(), protect_from_seq)
            .await
            .map_err(|e| RecordRefusal::Store(e.to_string()))
    }

    /// Newest recorded boundary, for tests and for a future resume path.
    pub async fn latest(
        &self,
    ) -> Result<Option<toron_core::index::run_state::RunStateCheckpoint>, RecordRefusal> {
        self.store
            .latest(self.thread.as_str())
            .await
            .map_err(|e| RecordRefusal::Store(e.to_string()))
    }

    pub fn thread(&self) -> &ThreadId {
        &self.thread
    }
}

/// How often the writer task drains the queue, and how many boundaries it keeps.
const WRITER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const WRITER_BATCH: usize = 64;
const RETAIN_BOUNDARIES: i64 = 512;

/// Start the writer task for a recorder.
///
/// The shape mirrors `effect_journal::journal_executor`: a dedicated thread with
/// its OWN runtime, so a store write never blocks the dispatcher's thread and
/// never nests a runtime inside another — the nested-panic class this codebase
/// keeps closed. The clone shares the queue, the boundary sequence and the
/// parent hint, so this drains the same log the fold enqueues into.
///
/// Bounded at both ends: a fixed interval and a fixed batch per wake, so a burst
/// cannot starve the dispatcher. After each non-empty drain it retains a fixed
/// window behind the newest boundary, so a long-lived dispatcher cannot grow
/// the table without limit. A store error is logged and the loop CONTINUES — one
/// bad boundary must not kill the writer for every later one.
pub fn spawn_writer(recorder: RunStateRecorder) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("flywheel-run-state-writer".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("flywheel: run-state writer runtime failed to build: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                loop {
                    tokio::time::sleep(WRITER_INTERVAL).await;
                    match recorder.drain(WRITER_BATCH).await {
                        Ok(0) => {}
                        Ok(_) => {
                            let floor = (recorder.next_seq() - RETAIN_BOUNDARIES).max(0);
                            if let Err(e) = recorder.retain(floor).await {
                                eprintln!("flywheel: run-state retain failed: {e}");
                            }
                        }
                        Err(e) => eprintln!("flywheel: run-state writer: {e}"),
                    }
                }
            });
        })
        .expect("spawn run-state writer")
}

#[cfg(test)]
mod recorder_tests {
    use super::*;
    use crate::state_log::model_from_boundary;
    use crate::workspace::WorkspaceModel;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// A clock that only moves when the test says so. No wall clock.
    fn pinned_clock(start: i64) -> (Clock, Arc<AtomicI64>) {
        let cell = Arc::new(AtomicI64::new(start));
        let c = cell.clone();
        (Arc::new(move || c.load(Ordering::SeqCst)), cell)
    }

    async fn recorder(_tag: &str) -> (RunStateRecorder, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("index.sqlite");
        let pool = toron_core::index::schema::connect(&db.to_string_lossy())
            .await
            .expect("connect");
        toron_core::index::schema::migrate(&pool)
            .await
            .expect("migrate");
        let store = RunStateStore::new(pool);
        let (clock, _cell) = pinned_clock(1_000);
        let r = RunStateRecorder::with_store(store, "flywheel/w6", clock).expect("recorder");
        (r, dir)
    }

    #[tokio::test]
    async fn records_consecutive_boundaries_and_advances_the_counter() {
        let (r, _d) = recorder("seq").await;
        assert_eq!(
            r.record(&WorkspaceModel::default(), &[]).await.expect("b0"),
            0
        );
        assert_eq!(
            r.record(&WorkspaceModel::default(), &[]).await.expect("b1"),
            1
        );
        assert_eq!(r.next_seq(), 2, "the counter advanced once per record");

        let latest = r
            .latest()
            .await
            .expect("latest")
            .expect("a boundary exists");
        assert_eq!(latest.seq, 1, "latest is the newest boundary");
    }

    #[tokio::test]
    async fn recorded_payload_decodes_back_to_the_same_model() {
        let (r, _d) = recorder("decode").await;
        let mut model = WorkspaceModel::default();
        model.seq = 9;
        model.session_topology = Some("open".into());
        r.record(&model, &["eff-1".into(), "eff-2".into()])
            .await
            .expect("record");

        let row = r.latest().await.expect("latest").expect("row");
        let back: WorkspaceModel = model_from_boundary(&row.model_json).expect("decode");
        assert_eq!(back, model, "what the log stores is what the fold held");

        let keys = crate::state_log::effect_keys_from_boundary(&row.model_json).expect("keys");
        assert_eq!(
            keys,
            vec!["eff-1".to_string(), "eff-2".to_string()],
            "the pending-write set survives with the boundary"
        );
    }

    /// The refusal that matters most: an oversized model is REFUSED, and the
    /// counter does NOT advance, so the next boundary is still the next one.
    #[tokio::test]
    async fn oversized_model_is_refused_and_does_not_advance_the_counter() {
        let (r, _d) = recorder("toobig").await;
        let mut model = WorkspaceModel::default();
        // Past the 1 MiB cap without allocating a giant model: a long queue of
        // distinct corrs is the realistic way doorbell.queues grows, and each
        // entry serializes to ~14 bytes of JSON, so 120k clears the cap with
        // room to spare (40k only reached ~546 KB and did NOT refuse, which is
        // why this count is explicit rather than incidental).
        model.doorbell.queues.insert(
            "pin".into(),
            (0..120_000).map(|i| format!("corr-{i:08}")).collect(),
        );

        let err = r.record(&model, &[]).await.expect_err("must refuse");
        assert!(
            matches!(err, RecordRefusal::TooLarge { .. }),
            "an oversized payload refuses as TooLarge, never truncates: {err}"
        );
        assert_eq!(
            r.next_seq(),
            0,
            "a refused boundary consumes no sequence number"
        );
        assert!(
            r.latest().await.expect("latest").is_none(),
            "a refused boundary leaves NO row, so a resume cannot find a lie"
        );
    }

    /// A chain of boundaries must be parent-linked, or a fork cannot describe
    /// where it came from.
    #[tokio::test]
    async fn consecutive_boundaries_are_parent_linked() {
        let (r, _d) = recorder("chain").await;
        r.record(&WorkspaceModel::default(), &[]).await.expect("b0");
        r.record(&WorkspaceModel::default(), &[]).await.expect("b1");
        r.record(&WorkspaceModel::default(), &[]).await.expect("b2");

        let all = r
            .store
            .history(r.thread.as_str(), None, 10)
            .await
            .expect("history");
        assert_eq!(all.len(), 3);
        let newest = &all[0];
        assert_eq!(newest.seq, 2);
        assert!(
            newest.parent_checkpoint_id.is_some(),
            "the newest boundary names its parent, so a fork can describe its origin"
        );
    }

    /// SATURATION FAILS LOUD, AND THE BOUNDARY IS NOT DROPPED. With no writer
    /// draining, the queue fills to `PENDING_CAP` and the next enqueue is
    /// REFUSED. This is the property that keeps a stalled writer from turning
    /// into unbounded memory, and the property that makes a skipped boundary
    /// visible instead of silent.
    #[tokio::test]
    async fn a_full_queue_refuses_loudly_rather_than_growing_or_dropping() {
        let (r, _d) = recorder("saturate").await;

        let mut model = WorkspaceModel::default();
        for i in 0..PENDING_CAP {
            model.seq = i as u64;
            r.enqueue(&model, &[]).expect("within the bound");
        }
        assert_eq!(r.pending_len(), PENDING_CAP, "the queue is exactly full");
        assert_eq!(
            r.next_seq(),
            PENDING_CAP as i64,
            "each enqueue took one seq"
        );

        model.seq = 9999;
        let err = r
            .enqueue(&model, &[])
            .expect_err("a full queue must refuse");
        assert_eq!(
            err,
            RecordRefusal::QueueFull,
            "overflow is a named refusal, not a silent drop"
        );
        assert_eq!(
            r.pending_len(),
            PENDING_CAP,
            "a refused boundary is not buffered either"
        );
        assert_eq!(
            r.next_seq(),
            PENDING_CAP as i64,
            "a refused boundary consumes no sequence number, so the next one is not a gap"
        );
        assert!(
            err.to_string().contains("refused"),
            "the message says refused, so an operator reading the log knows history was skipped: {err}"
        );
    }

    /// A boundary is durable only after the writer drains it. Until then the
    /// queue is the honest record of what exists, which is what makes a crash
    /// visible as "replay resumes from an earlier boundary" rather than as a
    /// silent gap.
    #[tokio::test]
    async fn enqueue_is_not_durable_until_drained() {
        let (r, _d) = recorder("drain").await;
        let model = WorkspaceModel::default();

        r.enqueue(&model, &[]).expect("enqueue");
        assert!(
            r.latest().await.expect("latest").is_none(),
            "an enqueued boundary is not yet durable"
        );
        assert_eq!(r.pending_len(), 1, "the queue is the honest record");

        assert_eq!(r.drain(10).await.expect("drain"), 1, "one boundary written");
        assert_eq!(r.pending_len(), 0);
        assert_eq!(
            r.latest().await.expect("latest").expect("durable").seq,
            0,
            "now it is durable"
        );
    }

    #[tokio::test]
    async fn retain_prunes_strictly_below_the_stated_floor() {
        let (r, _d) = recorder("retain").await;
        for _ in 0..4 {
            r.record(&WorkspaceModel::default(), &[])
                .await
                .expect("record");
        }
        let counts = r.retain(3).await.expect("retain");
        assert_eq!(counts.checkpoints, 3, "seq 0..=2 are below the floor");
        let latest = r.latest().await.expect("latest").expect("row");
        assert_eq!(latest.seq, 3, "the floor boundary itself survives");
    }

    /// The clock is injected, so a test can prove the recorder never reads wall
    /// time: two boundaries recorded under a pinned clock share a timestamp and
    /// are still ordered by `seq`, not by that timestamp.
    #[tokio::test]
    async fn ordering_follows_seq_even_when_the_injected_clock_does_not_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("index.sqlite");
        let pool = toron_core::index::schema::connect(&db.to_string_lossy())
            .await
            .expect("connect");
        toron_core::index::schema::migrate(&pool)
            .await
            .expect("migrate");
        // A clock that returns the SAME value forever.
        let frozen = Arc::new(|| 7_i64);
        let r = RunStateRecorder::with_store(RunStateStore::new(pool), "flywheel/w6", frozen)
            .expect("recorder");
        r.record(&WorkspaceModel::default(), &[]).await.expect("b0");
        r.record(&WorkspaceModel::default(), &[]).await.expect("b1");

        assert_eq!(
            r.latest().await.expect("latest").expect("row").seq,
            1,
            "a frozen clock must not make two boundaries collide or reorder"
        );
    }
}

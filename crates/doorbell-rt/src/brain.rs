// governed-by: ADR-0012
//! Brain — the single writer. Owns budgets, hear, cursors, digest, stats,
//! journal, and sinks. Loop: recv → [`Host::tick`] → journal(decided)
//! → execute effects → journal(out). File I/O is local and brief; the
//! brain never awaits a sink endpoint (shed, don't block) or the network
//! (sensors fetch off-loop and deliver results as messages).

use crate::api::Stats;
use crate::file_host::should_audit_count;
use crate::host::{Effect, Host};
use crate::msg::{AskKind, AskReply, BrainMsg};
use crate::sinks::{SendOutcome, SinkManager, Undelivered};
use crate::store::FileStore;
use bbtree::unsync::{BBTreeMap as BTreeMap, BBTreeSet as BTreeSet};
use doorbell_core::{PinBudget, take_allowed_like};

/// bd-5qyt: a journal append that failed. The row carries the seq the write
/// could not take and the running count, so a lost row NAMES itself instead of
/// surfacing later as an unattributable hole in the seq stream.
pub const AUDIT_JOURNAL_WRITE_FAILED: &str = "journal_write_failed";

/// bd-jcys: the D4.2 write-ahead flush failed. The decided row is IN the
/// journal but not on disk, so the prompt it describes can reach a sink with
/// nothing durable to explain it. Delivery is NOT withheld (see
/// `Brain::flush_journal`), so this row is the only trace — it cannot be
/// silent.
pub const AUDIT_JOURNAL_SYNC_FAILED: &str = "journal_sync_failed";

/// bd-jcys: a wake-state write failed. `persist_wake` already warned on its own
/// path; the two direct `save_wake` call sites discarded the error. One helper
/// now names all three, with the same sampled-row shape as the other repeats.
pub const AUDIT_WAKE_SAVE_FAILED: &str = "wake_save_failed";

/// bd-0uzrq.20: how long a structurally unroutable pin waits between inbox
/// probes. Its rows cannot become a ring (no covered pane), so fetching it
/// every sweep is pure cost: live 2026-11-15 showed one toron round trip every
/// ~3s for the captain's pin, each returning rows nobody could route.
pub(crate) const FETCH_BACKOFF_SECS: u64 = 60;

pub struct BrainConfig {
    pub workspace: String,
    pub digest_threshold: usize,
    pub digest_interval_secs: u64,
    pub main_sink: String,
    /// Owning subscription tag for journal rows (D5; default
    /// `workspace:{ws}` until the subscription store binds in .8+).
    pub subscriber: String,
    /// Per-subscriber budget override (D5 policy evaluation).
    pub budget: crate::subscribe::Budget,
}

impl BrainConfig {
    pub fn new(workspace: impl Into<String>) -> Self {
        let workspace = workspace.into();
        let subscriber = format!("workspace:{workspace}");
        Self {
            workspace,
            digest_threshold: 10,
            digest_interval_secs: 300,
            main_sink: "hc".into(),
            subscriber,
            budget: crate::subscribe::Budget::default_530(),
        }
    }
}

/// The event id of a watch line, when it carries one. Local to the brain:
/// the pure core keeps the time helpers, and the id-based dedup needs both
/// halves in one place to decide "same event, or merely same second".
fn watch_line_id(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

pub struct RunReport {
    pub undelivered: Vec<Undelivered>,
    pub stats: Stats,
}

/// bd-y40k: sampled liveness for the drain's straggler diagnosis. The brain
/// touches it on every select wake; the drain reads age + wake growth AT the
/// budget deadline, through a shared `Arc`. The clock is tokio's (substituted
/// under `start_paused`) and arrives as a parameter on read, so no probe path
/// samples wall time on its own.
pub struct BrainLiveness {
    epoch: tokio::time::Instant,
    last_wake_ms: std::sync::atomic::AtomicU64,
    wakes: std::sync::atomic::AtomicU64,
}

impl BrainLiveness {
    pub fn new() -> Self {
        Self {
            epoch: tokio::time::Instant::now(),
            last_wake_ms: std::sync::atomic::AtomicU64::new(0),
            wakes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Stamp the virtual clock and count the wake (relaxed atomics: the
    /// probe is diagnostic, never a synchronization point).
    pub fn touch(&self) {
        let ms = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_wake_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
        self.wakes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Milliseconds since the last select wake, read at the caller's `now`.
    pub fn age_ms(&self, now: tokio::time::Instant) -> u64 {
        let elapsed = u64::try_from(now.duration_since(self.epoch).as_millis()).unwrap_or(u64::MAX);
        elapsed.saturating_sub(self.last_wake_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn wakes(&self) -> u64 {
        self.wakes.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for BrainLiveness {
    fn default() -> Self {
        Self::new()
    }
}

/// No select wake for this long at the probe = the executor never scheduled
/// the task (bd-burk's live signature: silent for the whole 10s window).
pub const STARVATION_THRESHOLD_MS: u64 = 2_000;

/// bd-y40k's three-way straggler verdict: starvation wins on age (a stale
/// wake already answers "was it ever scheduled"); growth is only consulted
/// while the wake is recent, where it separates a churning backlog from a
/// task that woke once and went flat (the silent-park class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessVerdict {
    Starved { age_ms: u64 },
    Churning { age_ms: u64 },
    LostWake { age_ms: u64 },
}

impl LivenessVerdict {
    /// The straggler note renders these labels verbatim.
    pub fn describe(&self) -> String {
        match self {
            Self::Starved { age_ms } => format!(
                "starved: no select wake for {age_ms}ms (>= {STARVATION_THRESHOLD_MS}ms) — unscheduled, or a >2s dispatch (which logs its own warn)"
            ),
            Self::Churning { age_ms } => format!(
                "churning: last wake {age_ms}ms ago, wake count still growing — chewing queued work"
            ),
            Self::LostWake { age_ms } => format!(
                "lost-wake: last wake {age_ms}ms ago, count flat across the probe — parked, mid-exit-path, or mid-dispatch"
            ),
        }
    }
}

/// Pure classification (bd-y40k): age first, growth second.
pub fn classify_liveness(age_ms: u64, wakes_grew: bool) -> LivenessVerdict {
    if age_ms >= STARVATION_THRESHOLD_MS {
        LivenessVerdict::Starved { age_ms }
    } else if wakes_grew {
        LivenessVerdict::Churning { age_ms }
    } else {
        LivenessVerdict::LostWake { age_ms }
    }
}

/// `Some(next)` when the plane-S cursor moves. Same-second lines stay put
/// so a burst does not rewrite the wake file once per line.
pub(crate) fn watch_cursor_next(prev: Option<i64>, us: i64) -> Option<i64> {
    let next = prev.map(|c| c.max(us)).unwrap_or(us);
    if prev == Some(next) { None } else { Some(next) }
}

/// Fsync only to protect a prompt. A quiet watch must not `sync_data` the
/// whole journal: that stall fills the brain channel and the 5s wait drops.
pub(crate) fn watch_journal_due(pending_prompts: usize) -> bool {
    pending_prompts > 0
}

#[cfg(test)]
mod watch_batch_tests {
    use super::*;

    #[test]
    fn same_second_cursor_does_not_rewrite_and_a_prompt_syncs_now() {
        assert_eq!(watch_cursor_next(None, 5), Some(5));
        assert_eq!(watch_cursor_next(Some(5), 5), None);
        assert_eq!(watch_cursor_next(Some(5), 4), None);
        assert_eq!(watch_cursor_next(Some(5), 6), Some(6));
        assert!(!watch_journal_due(0));
        assert!(watch_journal_due(1));
    }
}

pub struct Brain<H: Host> {
    cfg: BrainConfig,
    host: H,
    store: FileStore,
    sinks: SinkManager,
    /// Budgets cross into `doorbell_core::take_allowed_like`, which takes
    /// std's map, and are keyed by pin (tens of rows), so this stays std.
    budgets: std::collections::BTreeMap<String, PinBudget>,
    hear: HearTracker,
    /// bd-jhz8x.2: when each pin's pane entered `blocked`, and the episodes
    /// already escalated (so one blocked bead rings once, not every sweep).
    blocked_since: std::collections::BTreeMap<String, u64>,
    escalated: doorbell_core::observe::EscalateSet,
    /// Forks seen in inbox rows since the last sweep (`[fork] <bead>: <q>`).
    pending_forks: Vec<doorbell_core::observe::ObserveEscalation>,
    /// bd-jhz8x.2 arm (b): unanswered peer request-release asks.
    release_waits: doorbell_core::observe::ReleaseWaits,
    wake: doorbell_core::WakeState,
    digest: Vec<(String, String)>,
    digest_since: Option<tokio::time::Instant>,
    stats: Stats,
    sweep_last: Option<u64>,
    /// Next journal seq (D4.4): resumed from the tail so rows stay
    /// monotonic per workspace across restarts.
    next_seq: u64,
    /// Deaths since the last restart that decided NOTHING (single-flight
    /// `restart_wake` suppressed them). Journaled at 1, 2, 4, 8 ... so a storm
    /// costs log2(deaths) rows instead of one per death, and the row states how
    /// many deaths it stands for. Reset on a real restart. Same shape as
    /// `dedup_high_us`: bounded sampling of a repeat, never silence.
    inert_died: u64,
    /// Journal appends that FAILED (bd-5qyt). A failed write must not consume a
    /// seq, so this is the tally of rows the journal still owes; the audit rows
    /// ride it for the row count and it names the seq that could not be written.
    journal_write_failed: u64,
    /// bd-jcys: write-ahead flushes that FAILED (see `flush_journal`). Same
    /// sampled shape as `journal_write_failed`: a repeat is a COUNT, not a row
    /// each, and the row names the context that failed to flush.
    journal_sync_failed: u64,
    /// bd-jcys: wake-state saves that FAILED (see `flush_wake`).
    wake_save_failed: u64,
    /// Sampled `watch` audit rows. One row per line filled the sidecar.
    watch_audits: u64,
    /// Consecutive unanswered expiries per pin (S3 deaf digest names
    /// subscriber + attempts; corr binds in bd-asr2.11 with receipts).
    deaf_attempts: BTreeMap<String, u32>,
    /// High-water of traced watch-dedup suppressions (bd-bpj1 F3): redelivered
    /// lines repeat identically (observed 82x per line), so only a suppression
    /// newer than every traced one earns a journal row — O(1) sampling that
    /// keeps the journal bounded while staying answerable.
    dedup_high_us: Option<i64>,
    /// Redelivery window: ids of watch lines already handled this process
    /// lifetime, in arrival order. The plane-S high-water cannot do this job
    /// (a second holds many events), and the window is what keeps the
    /// same-second case from reading as a redelivery. Bounded, because a
    /// forgotten id can only allow a duplicate ring, never hide a new one.
    watch_seen: std::collections::VecDeque<String>,
    watch_seen_set: BTreeSet<String>,
    /// bd-adms: the last ack-SLA expiry set `toron ack-sla` reported. A corr
    /// past SLA is re-reported on EVERY scan (live: the same 89 ids every
    /// 60s, forever), so only a CHANGE is news — a steady backlog is one
    /// fact, not one row per minute.
    sla_seen: BTreeSet<String>,
    /// bd-y40k: the shared probe the drain reads at its deadline — the
    /// brain touches it on every select wake, the drain classifies
    /// starve/churn/lost-wake from age + wake growth.
    liveness: std::sync::Arc<BrainLiveness>,
    /// Side signal for [`crate::msg::BrainMsg::Died`]. `None` keeps the
    /// channel-only path (tests that enqueue Died).
    died_bell: Option<std::sync::Arc<crate::msg::DiedBell>>,
    /// bd-0uzrq.20: last time the brain probed an unroutable pin's inbox. A
    /// pin with no prompt route is fetched at most once per
    /// [`FETCH_BACKOFF_SECS`]; the entry is dropped the moment a fetch routes.
    unroutable_park: BTreeMap<String, u64>,
    /// bd-0uzrq.20: the last unroutable row set already reported per pin, so a
    /// parked pin re-probing the same rows makes one journal row, not one per
    /// backoff window.
    unroutable_reported: BTreeMap<String, u64>,
}

impl<H: Host> Brain<H> {
    /// Wire: `let (tx, rx) = mpsc::channel(BRAIN_CAP);`
    /// `let sinks = SinkManager::new(tx.clone());`
    /// `Brain::new(cfg, host, store, sinks).run(rx).await`.
    pub fn new(cfg: BrainConfig, host: H, store: FileStore, sinks: SinkManager) -> Self {
        let wake = store.load_wake(&cfg.workspace);
        let mut hear = HearTracker::default();
        hear.seed_unanswered(store.load_unanswered(&cfg.workspace));
        // A seeded ring has no pending entry, so the only way the agent's
        // own reply clears it is the pin→npub map: fill it from the host
        // index at start (an unseeded restart otherwise kept a pin that had
        // already answered unanswered forever).
        for pin in hear.unanswered_pins() {
            hear.remember_npub(&pin, host.pin_npub(&pin).as_deref());
        }
        let next_seq = store.last_seq(&cfg.workspace) + 1;
        let mut budgets = std::collections::BTreeMap::new();
        for row in &store.fold_workspace(&cfg.workspace).rows {
            let is_budget = row
                .get("executed")
                .and_then(|e| e.get("stage"))
                .and_then(|s| s.as_str())
                == Some("budget");
            if !is_budget {
                continue;
            }
            let ts = row
                .get("ts")
                .and_then(|t| t.as_str())
                .and_then(|t| doorbell_core::parse_rfc3339_micros(t).ok())
                .map(|us| (us / 1_000_000) as u64)
                .unwrap_or(0);
            let grants: Vec<String> = row
                .get("executed")
                .and_then(|e| e.get("grants"))
                .and_then(|g| g.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            for pin in grants {
                budgets
                    .entry(pin)
                    .or_insert_with(|| {
                        PinBudget::new(cfg.budget.window_minutes, cfg.budget.max_prompts, ts)
                    })
                    .record_at(ts);
            }
        }
        let redelivered = count_journaled_prompts(&store, &cfg.workspace);
        let mut this = Self {
            cfg,
            host,
            store,
            sinks,
            budgets,
            hear,
            blocked_since: std::collections::BTreeMap::new(),
            escalated: doorbell_core::observe::EscalateSet::default(),
            pending_forks: Vec::new(),
            release_waits: doorbell_core::observe::ReleaseWaits::default(),
            wake,
            digest: Vec::new(),
            digest_since: None,
            stats: Stats::default(),
            sweep_last: None,
            next_seq,
            inert_died: 0,
            journal_write_failed: 0,
            journal_sync_failed: 0,
            wake_save_failed: 0,
            watch_audits: 0,
            deaf_attempts: BTreeMap::new(),
            dedup_high_us: None,
            watch_seen: std::collections::VecDeque::new(),
            watch_seen_set: BTreeSet::new(),
            sla_seen: BTreeSet::new(),
            liveness: std::sync::Arc::new(BrainLiveness::new()),
            died_bell: None,
            unroutable_park: BTreeMap::new(),
            unroutable_reported: BTreeMap::new(),
        };
        if redelivered > 0 {
            this.journal(
                &serde_json::json!({"rt_kind": "Resume"}),
                &[],
                serde_json::json!({"stage": "resume", "redelivered": redelivered}),
                &[],
            );
        }
        this
    }

    /// bd-y40k: share ONE probe between the brain's touches and the drain's
    /// deadline read (spawn wires the same Arc into both).
    pub fn with_liveness(mut self, liveness: std::sync::Arc<BrainLiveness>) -> Self {
        self.liveness = liveness;
        self
    }

    /// Death arrives on this bell, ahead of any watch already queued.
    pub fn with_died_bell(mut self, bell: std::sync::Arc<crate::msg::DiedBell>) -> Self {
        self.died_bell = Some(bell);
        self
    }

    /// bd-y40k: the handle the drain keeps for its straggler diagnosis.
    pub fn liveness(&self) -> std::sync::Arc<BrainLiveness> {
        self.liveness.clone()
    }

    pub async fn run(self, rx: tokio::sync::mpsc::Receiver<BrainMsg>) -> RunReport {
        self.run_with_status(rx, None).await
    }

    /// bd-0uzrq.17: the same loop with a SECOND, status-only lane polled
    /// `biased` first. Live 16:16-16:19Z a PlaneStatus took 76-155s to reach
    /// the journal because it queued behind mail-bound dispatches on the one
    /// channel; the wake it should have drained waited with it. Statuses are
    /// small, idempotent and edge-triggered, so letting them overtake mail is
    /// safe in the direction that matters — a drain never clears mail the fold
    /// has not seen (DeferredQueueLanes.tla's `drain-unreceived` fixture).
    pub async fn run_with_status(
        mut self,
        mut rx: tokio::sync::mpsc::Receiver<BrainMsg>,
        mut status_rx: Option<tokio::sync::mpsc::Receiver<BrainMsg>>,
    ) -> RunReport {
        let mut digest_tick = tokio::time::interval(std::time::Duration::from_secs(
            self.cfg.digest_interval_secs.max(1),
        ));
        digest_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let ws = self.cfg.workspace.clone();
        let started = tokio::time::Instant::now();
        let mut dispatched = 0usize;
        let elapsed_ms = || started.elapsed().as_millis();
        let died_bell = self.died_bell.clone();
        loop {
            self.liveness.touch();
            let died = async {
                match &died_bell {
                    Some(bell) => bell.wait().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                _ = died => {
                    let label = brain_msg_label(&BrainMsg::Died);
                    dispatched += 1;
                    let ws_d = ws.clone();
                    run_dispatch_with_slow_warn(label, self.dispatch(BrainMsg::Died), move |label| {
                        eprintln!(
                            "doorbell serve: brain {ws_d} dispatch {label} still running after {}s",
                            SLOW_DISPATCH_WARN.as_secs()
                        );
                    })
                    .await;
                }
                msg = async {
                    match status_rx.as_mut() {
                        Some(r) => r.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // A closed status lane is not the end of the brain: the
                    // poll thread can be restarted without losing mail.
                    let Some(msg) = msg else {
                        status_rx = None;
                        continue;
                    };
                    let label = brain_msg_label(&msg);
                    dispatched += 1;
                    let ws_s = ws.clone();
                    run_dispatch_with_slow_warn(label, self.dispatch(msg), move |label| {
                        eprintln!(
                            "doorbell serve: brain {ws_s} dispatch {label} still running after {}s",
                            SLOW_DISPATCH_WARN.as_secs()
                        );
                    })
                    .await;
                }
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    if matches!(msg, BrainMsg::Shutdown) {
                        eprintln!(
                            "{}",
                            ExitStep::ShutdownReceived.note(&ws, dispatched, elapsed_ms())
                        );
                        break;
                    }
                    let label = brain_msg_label(&msg);
                    dispatched += 1;
                    let ws_d = ws.clone();
                    run_dispatch_with_slow_warn(label, self.dispatch(msg), move |label| {
                        eprintln!(
                            "doorbell serve: brain {ws_d} dispatch {label} still running after {}s",
                            SLOW_DISPATCH_WARN.as_secs()
                        );
                    })
                    .await;
                }
                _ = digest_tick.tick() => {
                    let ws_t = ws.clone();
                    run_dispatch_with_slow_warn(
                        "digest-flush",
                        self.maybe_flush_digest(),
                        move |label| {
                            eprintln!(
                                "doorbell serve: brain {ws_t} dispatch {label} still running after {}s",
                                SLOW_DISPATCH_WARN.as_secs()
                            );
                        },
                    )
                    .await;
                }
            }
        }
        eprintln!(
            "{}",
            ExitStep::FlushDigest.note(&ws, dispatched, elapsed_ms())
        );
        self.maybe_flush_digest().await;
        eprintln!(
            "{}",
            ExitStep::SinkShutdown.note(&ws, dispatched, elapsed_ms())
        );
        let undelivered = self.sinks.shutdown().await;
        if !undelivered.is_empty() {
            eprintln!(
                "{}",
                ExitStep::JournalUndelivered.note(&ws, dispatched, elapsed_ms())
            );
            let pins: Vec<String> = undelivered.iter().map(|u| u.pin.clone()).collect();
            self.journal(
                &serde_json::json!({"rt_kind": "Shutdown"}),
                &[],
                serde_json::json!({"stage": "drain", "undelivered": undelivered.len()}),
                &pins,
            );
        }
        eprintln!("{}", ExitStep::Complete.note(&ws, dispatched, elapsed_ms()));
        RunReport {
            undelivered,
            stats: self.stats.clone(),
        }
    }

    async fn dispatch(&mut self, msg: BrainMsg) {
        match msg {
            BrainMsg::Watch(line) => self.on_watch(line).await,
            BrainMsg::PlaneStatus { pin, status } => self.on_status(pin, status).await,
            BrainMsg::Died => self.on_died().await,
            BrainMsg::Sweep { now } => self.on_sweep(now).await,
            BrainMsg::RoleBound { role, pin } => self.on_rolebound(role, pin).await,
            BrainMsg::Fact { summary } => self.on_fact(summary).await,
            BrainMsg::AckSlaTick { now } => self.on_ack_sla(now).await,
            BrainMsg::Presence { pin, state } => {
                let effects = self.host.tick(&BrainMsg::Presence {
                    pin: pin.clone(),
                    state: state.clone(),
                });
                self.deliver(effects, "presence").await;
            }
            BrainMsg::PromptRefused { pin, line } => {
                let effects = self.host.tick(&BrainMsg::PromptRefused {
                    pin: pin.clone(),
                    line: line.clone(),
                });
                self.deliver(effects, "prompt-refused").await;
            }
            BrainMsg::EventsHint {
                event_id,
                pin,
                line,
            } => {
                self.on_events(
                    &BrainMsg::EventsHint {
                        event_id,
                        pin,
                        line,
                    },
                    "events-hint",
                )
                .await;
            }
            BrainMsg::EventsGap { pin } => {
                self.on_events(&BrainMsg::EventsGap { pin }, "events-gap")
                    .await;
            }
            BrainMsg::InboxFetched { pin, rows } => self.on_inbox_fetched(pin, rows).await,
            BrainMsg::SlaFired { posts } => self.on_sla_fired(posts).await,
            BrainMsg::Degraded { source, reason } => {
                self.stats.degraded += 1;
                self.audit(
                    "degraded",
                    serde_json::json!({ "source": source, "reason": reason }),
                );
                self.journal(
                    &serde_json::json!({"rt_kind": "Degraded", "source": source}),
                    &[],
                    serde_json::json!({"stage": "degraded", "reason": reason}),
                    &[],
                );
                if let Some(sink) = source.strip_prefix("sink:") {
                    let now = self.host.now_unix();
                    if self.sinks.restart_sink(sink, now).await {
                        let n = self.sinks.requeue_shed_overflow(sink).await;
                        if n > 0 {
                            self.audit(
                                "sink_restarted",
                                serde_json::json!({ "sink": sink, "requeued": n }),
                            );
                        }
                    } else {
                        for u in self.sinks.drain_shed_overflow().await {
                            self.stats.sheds += 1;
                            self.audit(
                                "shed_overflow",
                                serde_json::json!({ "sink": u.sink, "pin": u.pin }),
                            );
                            self.digest_push(u.pin, u.line);
                        }
                    }
                } else {
                    for u in self.sinks.drain_shed_overflow().await {
                        self.stats.sheds += 1;
                        self.audit(
                            "shed_overflow",
                            serde_json::json!({ "sink": u.sink, "pin": u.pin }),
                        );
                        self.digest_push(u.pin, u.line);
                    }
                }
                self.maybe_flush_digest().await;
            }
            BrainMsg::Delivered { pin } => {
                self.hear.note_inject(
                    &pin,
                    self.host.pin_npub(&pin).as_deref(),
                    self.host.now_unix(),
                );
                self.persist_hear();
            }
            BrainMsg::Ask { kind, reply } => {
                let _ = reply.send(match kind {
                    AskKind::Stats => AskReply::Stats(self.snapshot_stats()),
                    AskKind::Digest => {
                        let digest = std::mem::take(&mut self.digest);
                        self.digest_since = None;
                        AskReply::Digest(digest)
                    }
                });
            }
            BrainMsg::Shutdown => {}
        }
    }

    async fn on_watch(&mut self, line: String) {
        let id = watch_line_id(&line);
        let redelivered = id
            .as_deref()
            .is_some_and(|id| self.watch_seen_set.contains(id));
        let us = doorbell_core::watch_created_at(&line).map(|c| (c as i64) * 1_000_000);
        let history = us.is_some_and(|us| us < self.wake.plane_s_cursor_us.unwrap_or(i64::MIN));
        if redelivered || history {
            *self
                .stats
                .rings_by_class
                .entry("watch_dedup".into())
                .or_default() += 1;
            let ring_candidate = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v.get("kind").and_then(|k| k.as_u64()))
                .is_none_or(|k| k != 20001);
            if let (Some(us), true) = (us, ring_candidate)
                && self.dedup_high_us.is_none_or(|h| us > h)
            {
                self.dedup_high_us = Some(us);
                self.journal(
                    &serde_json::json!({"rt_kind": "Watch"}),
                    &[],
                    serde_json::json!({"stage": "dedup", "cursor_us": us}),
                    &[],
                );
            }
            return;
        }
        if let Some(us) = us
            && let Some(next) = watch_cursor_next(self.wake.plane_s_cursor_us, us)
        {
            self.wake.plane_s_cursor_us = Some(next);
            self.flush_wake("watch-cursor");
        }
        if let Some(id) = id {
            self.remember_watch_id(id);
        }
        *self.stats.rings_by_class.entry("watch".into()).or_default() += 1;
        let effects = self.host.tick(&BrainMsg::Watch(line.clone()));
        self.absorb_digests(&effects);
        let raw: Vec<(String, String)> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Prompt { pin, line, .. } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        let now = self.host.now_unix();
        let injects = self.gated(raw.clone(), now);
        let (class, targets) = self.host.doorbell_index().map_or((None, 0), |idx| {
            (
                doorbell_core::parse_watch_ndjson(&line, idx).map(|(c, _)| format!("{c:?}")),
                doorbell_core::watch_targets(&line, idx).len(),
            )
        });
        let mut effect = serde_json::json!({
            "allowed": injects.len(), "gated": raw.len().saturating_sub(injects.len()),
            "targets": targets,
        });
        if let Some(c) = class {
            effect["class"] = serde_json::Value::String(c);
        }
        let pins: Vec<String> = raw.iter().map(|(p, _)| p.clone()).collect();
        self.journal(
            &serde_json::json!({"rt_kind": "Watch"}),
            &effects,
            serde_json::json!({"stage": "decided", "effect": effect}),
            &pins,
        );
        if raw.len() != injects.len() {
            self.stats.drops += (raw.len() - injects.len()) as u64;
            self.audit(
                "budget_drop",
                serde_json::json!({ "dropped": raw.len() - injects.len() }),
            );
        }
        if crate::file_host::should_audit_count(self.watch_audits as usize) {
            self.audit("watch", serde_json::json!({"n": self.watch_audits + 1}));
        }
        self.watch_audits = self.watch_audits.saturating_add(1);
        if let Some(pk) = doorbell_core::watch_pubkey(&line) {
            self.hear.note_heard_pubkey(&pk);
            self.persist_hear();
        }
        if watch_journal_due(injects.len()) {
            self.flush_journal("watch");
        }
        let delivered = self.deliver_prompts(injects).await;
        let woke = delivered.first().cloned();
        self.persist_wake(woke.as_deref(), "watch");
    }

    /// One events hint/gap through the same single budget gate as a watch ring
    /// (bd-l5vb2). Journal the decided effects BEFORE delivering any prompt,
    /// so a crash replays from the persisted cursor and the bus event id
    /// deduper absorbs the duplicate.
    async fn on_events(&mut self, msg: &BrainMsg, stage: &str) {
        let effects = self.host.tick(msg);
        self.absorb_digests(&effects);
        let raw = self.prompts_of(&effects);
        let now = self.host.now_unix();
        let injects = self.gated(raw.clone(), now);
        let pins: Vec<String> = raw.iter().map(|(p, _)| p.clone()).collect();
        self.journal(
            &serde_json::json!({"rt_kind": "Events"}),
            &effects,
            serde_json::json!({
                "stage": stage,
                "allowed": injects.len(),
                "gated": raw.len().saturating_sub(injects.len()),
            }),
            &pins,
        );
        if raw.len() != injects.len() {
            self.stats.drops += (raw.len() - injects.len()) as u64;
            self.audit(
                "budget_drop",
                serde_json::json!({ "dropped": raw.len() - injects.len() }),
            );
        }
        self.flush_journal("events");
        let delivered = self.deliver_prompts(injects).await;
        self.persist_wake(delivered.first().map(String::as_str), "events");
    }

    async fn on_status(&mut self, pin: String, status: String) {
        let from = self.host.prev_status(&pin).map(|f| f.to_ascii_lowercase());
        let effects = self.host.tick(&BrainMsg::PlaneStatus {
            pin: pin.clone(),
            status: status.clone(),
        });
        self.absorb_digests(&effects);
        let raw = self.prompts_of(&effects);
        let now = self.host.now_unix();
        let injects = self.gated(raw.clone(), now);
        self.journal(
            &serde_json::json!({
                "rt_kind": "PlaneStatus", "pin": pin, "status": status.as_str()
            }),
            &effects,
            serde_json::json!({"stage": "drain"}),
            &raw.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
        );
        self.flush_journal("status");
        let delivered = self.deliver_prompts(injects).await;
        let s = status.to_ascii_lowercase();
        // bd-jhz8x.2 arm (a): the clock starts when the pane ENTERS blocked and
        // is cleared the moment it leaves — a flapping pane never accumulates.
        if s == "blocked" {
            self.blocked_since
                .entry(pin.clone())
                .or_insert_with(|| self.host.now_unix());
        } else {
            self.blocked_since.remove(&pin);
        }
        let transitioned = match from.as_deref() {
            Some(f) => f != s.as_str(),
            None => true,
        };
        let observed =
            transitioned && matches!(s.as_str(), "working" | "idle" | "done" | "blocked");
        let woke = if observed {
            Some(pin.clone())
        } else {
            delivered.first().cloned()
        };
        self.persist_wake(woke.as_deref(), "status");
    }

    async fn on_died(&mut self) {
        let effects = self.host.tick(&BrainMsg::Died);
        let restarted = self.host.restart_wake();
        let decided: Vec<Effect> = if restarted {
            effects
        } else {
            effects
                .into_iter()
                .filter(|effect| !matches!(effect, Effect::RestartWatch))
                .collect()
        };
        if !restarted {
            self.inert_died += 1;
        }
        let quiet = !restarted && !self.inert_died.is_power_of_two();
        if !quiet {
            let mut executed = serde_json::json!({"stage": "executed", "restarted": restarted});
            if !restarted {
                executed["coalesced_deaths"] = serde_json::json!(self.inert_died);
            }
            self.journal(
                &serde_json::json!({"rt_kind": "Died"}),
                &decided,
                executed,
                &[],
            );
        }
        if restarted {
            self.inert_died = 0;
        }
        let _ = self
            .hear
            .expire(self.host.now_unix(), doorbell_core::UNANSWERED_SECS);
        self.persist_hear();
    }

    async fn on_rolebound(&mut self, role: String, pin: String) {
        let effects = self.host.tick(&BrainMsg::RoleBound {
            role: role.clone(),
            pin: pin.clone(),
        });
        self.journal(
            &serde_json::json!({"rt_kind": "RoleBound", "role": role}),
            &effects,
            serde_json::json!({"stage": "role_bound_live", "role": role, "pin": pin}),
            std::slice::from_ref(&pin),
        );
        self.deliver(effects, "role_bound").await;
    }

    async fn on_fact(&mut self, summary: String) {
        let effects = self.host.tick(&BrainMsg::Fact {
            summary: summary.clone(),
        });
        self.journal(
            &serde_json::json!({"rt_kind": "Fact"}),
            &effects,
            serde_json::json!({"stage": "fact_live", "summary": summary}),
            &self.pins_of(&effects),
        );
        self.deliver(effects, "fact").await;
    }
    async fn on_sweep(&mut self, now: u64) {
        for (role, pin) in self.host.drain_role_bounds() {
            self.on_rolebound(role, pin).await;
        }
        for summary in self.host.drain_facts() {
            self.on_fact(summary).await;
        }
        for (pin, state) in self.host.sweep_maintenance() {
            let effects = self.host.tick(&BrainMsg::Presence {
                pin: pin.clone(),
                state: state.clone(),
            });
            self.deliver(effects, "presence").await;
            self.hear.note_heard_pin(&pin);
        }
        self.sweep_last = Some(now);
        let effects = self.host.tick(&BrainMsg::Sweep { now });
        if effects.iter().any(|e| matches!(e, Effect::FetchInbox)) {
            let targets = self.fetch_targets(now);
            let inline = self.host.fetch_inbox_detached(targets);
            for (pin, rows) in inline {
                self.on_inbox_fetched(pin, rows).await;
            }
        }
        if is_decision(&effects) {
            self.journal(
                &serde_json::json!({"rt_kind": "Sweep"}),
                &effects,
                serde_json::json!({"stage": "sweep"}),
                &[],
            );
        }
        let _ = self.store.touch_sweep(&self.cfg.workspace, now);
        self.deliver(effects, "sweep").await;
        for pin in self.hear.expire(now, doorbell_core::UNANSWERED_SECS) {
            *self.deaf_attempts.entry(pin).or_insert(0) += 1;
        }
        // A pin with no pane cannot hear: resolve its ring instead of letting
        // it age into a permanent conflict (bd-3vkpe). Only an authoritative
        // live set may drop a ring; a host that cannot tell keeps them all.
        let resolved = match self.host.live_pins() {
            Some(live) => self.hear.retain_live(&live),
            None => Vec::new(),
        };
        for pin in resolved {
            self.deaf_attempts.remove(&pin);
            self.journal(
                &serde_json::json!({"rt_kind": "RingResolved"}),
                &[],
                serde_json::json!({"stage": "ring_resolved", "pin": pin, "why": "no pane, nothing to ring"}),
                std::slice::from_ref(&pin),
            );
        }
        self.emit_observe_escalations(now).await;
        let unanswered = self.hear.unanswered_pins();
        self.deaf_attempts.retain(|pin, _| unanswered.contains(pin));
        // Post-start hear-map gap: the host index learns npubs after
        // `Brain::new` (the registry refresh lands in `sweep_maintenance`
        // above), so a seeded unanswered pin's map entry stays empty until a
        // sweep refreshes it — without this its late reply never clears it.
        // Bounded to unanswered pins (no registry-wide scan); the
        // `persist_hear` below already fires every sweep, so no write is added.
        for pin in unanswered {
            self.hear
                .remember_npub(&pin, self.host.pin_npub(&pin).as_deref());
        }
        self.persist_hear();
    }

    /// bd-jhz8x.2 (ADR-0021 D4.5): the observe arms, in the engine. One line per
    /// (kind, subject) EPISODE, addressed to the captain. A host that cannot
    /// name the captain gets a journaled audit instead — never a silent drop.
    async fn emit_observe_escalations(&mut self, now: u64) {
        use doorbell_core::observe::{BLOCKED_AFTER_SECS, ObserveEscalation};

        let mut escalations: Vec<ObserveEscalation> = Vec::new();
        let mut blocked: Vec<(&String, u64)> = self
            .blocked_since
            .iter()
            .map(|(pin, since)| (pin, now.saturating_sub(*since)))
            .filter(|(_, age)| *age >= BLOCKED_AFTER_SECS)
            .collect();
        blocked.sort();
        for (pin, age) in blocked {
            escalations.push(ObserveEscalation::blocked_bead(
                pin,
                pin,
                age,
                "unblock a blocked pane earlier: split the step, or mark the fork before it blocks",
            ));
        }
        escalations.append(&mut self.pending_forks);
        escalations.extend(self.release_waits.expired(now));

        let fresh = self
            .escalated
            .new_since(&escalations)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if fresh.is_empty() {
            return;
        }
        let captain = self.host.captain_pin();
        let mut effects = Vec::new();
        for escalation in &fresh {
            let line = escalation.line();
            match &captain {
                Some(pin) => effects.push(Effect::Prompt {
                    event_id: doorbell_core::watch_id(&line),
                    pin: pin.clone(),
                    line,
                }),
                None => effects.push(Effect::Audit {
                    kind: "observe-escalation".into(),
                    pin: escalation.subject.clone(),
                    detail: serde_json::json!({
                        "reason": "no captain bound for this project",
                        "line": line,
                    }),
                }),
            }
        }
        self.journal(
            &serde_json::json!({"rt_kind": "Observe"}),
            &effects,
            serde_json::json!({"stage": "escalate", "count": fresh.len()}),
            &fresh.iter().map(|e| e.subject.clone()).collect::<Vec<_>>(),
        );
        self.deliver(effects, "observe").await;
    }

    async fn on_inbox_fetched(&mut self, pin: String, rows: Vec<serde_json::Value>) {
        // bd-jhz8x.2 arm (c): an explicit fork is a mail SUBJECT of
        // `[fork] <bead>: <question>`. Parks until the next sweep so the
        // episode rule and the captain address live in one place.
        for row in &rows {
            let Some(subject) = row.get("subject").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if let Some((bead, question)) = doorbell_core::observe::fork_subject(subject) {
                self.pending_forks
                    .push(doorbell_core::observe::ObserveEscalation::fork(
                        bead, question,
                    ));
            }
        }
        // Arm (b): asks this pin holds, and answers to its own asks (a
        // release-response lands in the REQUESTER's inbox, so the fetch pin is
        // the requester the answer clears).
        for row in &rows {
            if let Some(wait) = doorbell_core::observe::release_request(row, &pin) {
                self.release_waits.note_request(wait);
            } else if let Some(path) = doorbell_core::observe::release_answer(row) {
                self.release_waits.note_answered(&path, &pin);
            }
        }
        let fetched = !rows.is_empty();
        let refs: Vec<&serde_json::Value> = rows.iter().collect();
        let hw = doorbell_core::inbox_high_water_from_values(refs.clone());
        // A wake row must not be consumed while it cannot be routed: the
        // queued line lives only in the fold's model, and a host restart
        // would lose it after the cursor moved past it. Advance the cursor
        // only when the fetch carried no wake rows, or when the fold
        // actually decided (Prompt/Digest). A stuck pin re-fetches until it
        // becomes injectable — bounded, and no mail is eaten silently.
        let wake_rows = refs
            .iter()
            .filter(|r| doorbell_core::parse_inbox_item(r).is_some())
            .count();
        let rows_digest = inbox_rows_digest(&rows);
        let effects = self.host.tick(&BrainMsg::InboxFetched {
            pin: pin.clone(),
            rows,
        });
        let decided = effects
            .iter()
            .any(|e| matches!(e, Effect::Prompt { .. } | Effect::Digest { .. }));
        // A queued wake is HELD by the fold (drained on the next idle
        // status): that is a decision, and holding the cursor here made the
        // sweep re-fetch the same rows every few seconds (observed live as a
        // 3s toron-CLI hot loop). Advance once the line is queued.
        let queued = effects
            .iter()
            .any(|e| matches!(e, Effect::Audit { kind, .. } if kind == "queued"));
        // bd-0uzrq.20: rows that produced neither a ring nor a queue entry can
        // never become one — no later sweep decides them. For a pin with no
        // prompt route (the captain: a subscriber with no covered pane) that
        // used to hold the cursor, so every Sweep refetched the same rows: one
        // toron round trip every ~3s, forever. Report the row set ONCE and park
        // the pin; the park is what bounds the probes. A routable pin keeps the
        // old hold — its wake may still become deliverable.
        let mut effects = effects;
        let stuck = wake_rows > 0 && !decided && !queued;
        let unroutable = stuck && !self.host.has_route(&pin);
        if unroutable && self.unroutable_reported.get(&pin) != Some(&rows_digest) {
            self.unroutable_reported.insert(pin.clone(), rows_digest);
            effects.push(Effect::Audit {
                kind: "unroutable".into(),
                pin: pin.clone(),
                detail: serde_json::json!({
                    "reason": "no prompt route; rows stay unread in the mailbox",
                    "rows": wake_rows,
                    "backoff_secs": FETCH_BACKOFF_SECS,
                }),
            });
        }
        if let Some(hw) = hw
            && (wake_rows == 0 || decided || queued || unroutable)
        {
            self.wake.since_by_pin.insert(pin.clone(), hw);
            self.flush_wake("inbox-cursor");
        }
        if decided || queued {
            self.unroutable_park.remove(&pin);
            self.unroutable_reported.remove(&pin);
        }
        if fetched || is_decision(&effects) {
            self.journal(
                &serde_json::json!({"rt_kind": "InboxFetched", "pin": pin}),
                &effects,
                serde_json::json!({"stage": "executed"}),
                &[pin],
            );
        }
        self.deliver(effects, "inbox").await;
    }

    fn fetch_targets(&mut self, now_unix: u64) -> Vec<(String, String)> {
        let floor = doorbell_core::redelivery_floor_rfc3339(now_unix);
        let mut targets = Vec::new();
        for pin in self.host.pins() {
            if !self.host.has_route(&pin) {
                let due = self
                    .unroutable_park
                    .get(&pin)
                    .is_none_or(|at| now_unix.saturating_sub(*at) >= FETCH_BACKOFF_SECS);
                if !due {
                    continue;
                }
                // Hand out the probe and start the next window now, so an
                // empty probe (the common case once the cursor has advanced)
                // cannot put this pin back on the every-sweep path.
                self.unroutable_park.insert(pin.clone(), now_unix);
            }
            let pin_since = self
                .wake
                .since_by_pin
                .get(&pin)
                .cloned()
                .unwrap_or_else(|| floor.clone());
            targets.push((pin, pin_since));
        }
        targets
    }

    async fn on_ack_sla(&mut self, now: u64) {
        let effects = self.host.tick(&BrainMsg::AckSlaTick { now });
        let scan = effects.iter().any(|e| matches!(e, Effect::CheckAckSla));
        self.journal(
            &serde_json::json!({"rt_kind": "AckSlaTick"}),
            &effects,
            serde_json::json!({"stage": "executed", "scanned": scan}),
            &[],
        );
        if scan {
            let posts = self.host.scan_ack_sla_detached();
            self.on_sla_fired(posts).await;
        }
        if effects.iter().any(|e| matches!(e, Effect::FetchInbox)) {
            let targets = self.fetch_targets(now);
            let inline = self.host.fetch_inbox_detached(targets);
            for (pin, rows) in inline {
                self.on_inbox_fetched(pin, rows).await;
            }
        }
        self.deliver(effects, "acksla").await;
    }
    async fn on_sla_fired(&mut self, posts: Vec<String>) {
        let current: BTreeSet<String> = posts.iter().cloned().collect();
        if current == self.sla_seen {
            return;
        }
        let cleared = self.sla_seen.len();
        self.sla_seen = current;
        if posts.is_empty() {
            if cleared > 0 {
                self.audit("ack_sla_cleared", serde_json::json!({ "cleared": cleared }));
            }
            return;
        }
        self.audit("ack_sla_fired", serde_json::json!({ "count": posts.len() }));
        let effects = self.host.tick(&BrainMsg::SlaFired {
            posts: posts.clone(),
        });
        self.journal(
            &serde_json::json!({"rt_kind": "SlaFired", "count": posts.len()}),
            &effects,
            serde_json::json!({"stage": "executed"}),
            &self.pins_of(&effects),
        );
        let fetch = effects.iter().any(|e| matches!(e, Effect::FetchInbox));
        self.deliver(effects, "sla").await;
        if fetch {
            let targets = self.fetch_targets(self.host.now_unix());
            let inline = self.host.fetch_inbox_detached(targets);
            for (pin, rows) in inline {
                self.on_inbox_fetched(pin, rows).await;
            }
        }
    }

    /// Budget gate + sink delivery shared by every arm. Returns delivered pins.
    /// Audit/FactMiss/LoudMiss/Digest effects are recorded by the Host layer
    /// (single ownership); the journal row above already carries them as data.
    async fn deliver(&mut self, effects: Vec<Effect>, _stage: &str) -> Vec<String> {
        self.absorb_digests(&effects);
        let raw = self.prompts_of(&effects);
        if raw.is_empty() {
            self.maybe_flush_digest().await;
            return Vec::new();
        }
        let now = self.host.now_unix();
        let injects = self.gated(raw.clone(), now);
        if raw.len() != injects.len() {
            self.stats.drops += (raw.len() - injects.len()) as u64;
        }
        self.flush_journal("deliver");
        self.deliver_prompts(injects).await
    }

    /// Per-subscriber budget gate (D5): new pins inherit the owning
    /// subscription's template instead of flat 5/30/3. Every grant set is
    /// journaled as a budget row (D4.5) so crash-resume refolds exact state.
    fn gated(&mut self, raw: Vec<(String, String)>, now: u64) -> Vec<(String, String)> {
        let b = self.cfg.budget;
        let allowed =
            take_allowed_like(raw, &mut self.budgets, now, b.window_minutes, b.max_prompts);
        if !allowed.is_empty() {
            let grants: Vec<String> = allowed.iter().map(|(p, _)| p.clone()).collect();
            self.journal(
                &serde_json::json!({"rt_kind": "Budget"}),
                &[],
                serde_json::json!({
                    "stage": "budget",
                    "grants": grants,
                    "window_minutes": b.window_minutes,
                    "max_prompts": b.max_prompts,
                }),
                &grants,
            );
        }
        allowed
    }

    /// bd-8ulu: the sink a wake for `pin` goes to.
    ///
    /// The subscription row's `sink` is the operator's explicit choice
    /// (identity x scope x sink), so it outranks the workspace-wide
    /// `main_sink`; `main_sink` remains the fallback so every single-sink
    /// deployment — and every host that does not implement [`Host::sink_for`]
    /// — behaves exactly as before.
    fn sink_for(&self, pin: &str) -> String {
        self.host
            .sink_for(pin)
            .unwrap_or_else(|| self.cfg.main_sink.clone())
    }

    /// Does a wake for `pin` on `sink` have anywhere to land?
    ///
    /// bd-bk9t made "no prompt route" mean "keep it in the digest" because
    /// the hc sink MISSES an unknown pin and still reports `Ok`, so
    /// flushing would stamp a `Delivered` receipt for a wake nobody received.
    /// A registered non-pane sink is the opposite case: the webhook endpoint
    /// IS the destination, so the pane-less pin finally has a route
    /// (bd-8ulu). An unregistered name is not a route — the send would be
    /// absorbed by nobody.
    fn route_exists(&mut self, pin: &str, sink: &str) -> bool {
        self.host.has_route(pin) || self.sinks.has(sink)
    }

    async fn deliver_prompts(&mut self, injects: Vec<(String, String)>) -> Vec<String> {
        let mut queued = Vec::new();
        for (pin, line) in injects {
            if !self.host.inject_allowed(&pin) {
                continue;
            }
            let sink = self.sink_for(&pin);
            match self.sinks.send(&sink, &pin, &line).await {
                crate::sinks::SendOutcome::Queued => {
                    self.stats.injects += 1;
                    queued.push(pin);
                }
                crate::sinks::SendOutcome::Shed { pin, line } => {
                    self.stats.sheds += 1;
                    self.digest_push(pin, line);
                }
                crate::sinks::SendOutcome::Absent => {
                    self.audit("sink_absent", serde_json::json!({ "sink": sink }));
                }
            }
        }
        self.maybe_flush_digest().await;
        queued
    }

    /// Fold every `Effect::Digest` into the batch (bd-bk9t). Every arm that
    /// consumes effects calls this before its prompt path, so a wake with no
    /// prompt route still lands in the human-visible digest. The flush is
    /// left to the arm (`deliver_prompts` ends with one unconditionally).
    fn absorb_digests(&mut self, effects: &[Effect]) {
        for e in effects {
            if let Effect::Digest { pin, line, .. } = e {
                self.digest_push(pin.clone(), line.clone());
            }
        }
    }

    fn digest_push(&mut self, pin: String, line: String) {
        if self.digest_since.is_none() {
            self.digest_since = Some(tokio::time::Instant::now());
        }
        self.digest.push((pin, line));
    }

    async fn maybe_flush_digest(&mut self) {
        let due_len = self.digest.len() >= self.cfg.digest_threshold;
        let due_time = self
            .digest_since
            .is_some_and(|t| t.elapsed().as_secs() >= self.cfg.digest_interval_secs);
        if !(due_len || due_time) {
            return;
        }
        let mut by_pin: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (pin, line) in std::mem::take(&mut self.digest) {
            by_pin.entry(pin).or_default().push(line);
        }
        for pin in self.hear.unanswered_pins() {
            if let Some(n) = self.deaf_attempts.get(&pin) {
                by_pin.entry(pin.clone()).or_default().push(format!(
                    "[deaf] subscriber={} attempts={n} unanswered",
                    self.cfg.subscriber,
                ));
            }
        }
        if by_pin.is_empty() {
            return;
        }
        self.digest_since = None;
        self.stats.digest_batches += 1;
        for (pin, lines) in by_pin {
            let sink = self.sink_for(&pin);
            if !self.route_exists(&pin, &sink) {
                continue;
            }
            let batch = format!("[digest] {} queued: {}", lines.len(), lines.join(" | "));
            match self.sinks.send(&sink, &pin, &batch).await {
                SendOutcome::Queued => {}
                SendOutcome::Shed { pin, line } => {
                    self.digest_push(pin, line);
                }
                SendOutcome::Absent => {}
            }
        }
    }

    fn journal(
        &mut self,
        msg: &serde_json::Value,
        effects: &[Effect],
        executed: serde_json::Value,
        pins: &[String],
    ) {
        let actions = serde_json::json!(
            effects
                .iter()
                .map(|e| match e {
                    Effect::Prompt { pin, line, .. } => {
                        serde_json::json!({"Prompt": {"pin": pin, "line": line}})
                    }
                    Effect::FetchInbox => {
                        serde_json::json!("FetchInbox")
                    }
                    Effect::CheckAckSla => serde_json::json!("CheckAckSla"),
                    Effect::RestartWatch => serde_json::json!("RestartWatch"),
                    Effect::Audit { kind, pin, detail } => {
                        serde_json::json!({"Audit": {"kind": kind, "pin": pin, "detail": detail}})
                    }
                    Effect::Digest { pin, line, why } => serde_json::json!({
                        "Digest": {"pin": pin, "line": line, "why": why}
                    }),
                })
                .collect::<Vec<_>>()
        );
        let seq = self.next_seq;
        let written = self.store.append_journal_seq(
            &self.cfg.workspace,
            msg,
            &actions,
            executed,
            pins,
            Some(seq),
            Some(self.cfg.subscriber.as_str()),
        );
        match written {
            Ok(()) => self.next_seq += 1,
            Err(e) => {
                self.journal_write_failed += 1;
                eprintln!(
                    "doorbell-rt: journal append FAILED on {} — refuse silent drop (GS-039), \
                     seq {seq} stays owed: {e:#}",
                    self.cfg.workspace
                );
                if should_audit_count(self.journal_write_failed as usize - 1) {
                    self.audit(
                        AUDIT_JOURNAL_WRITE_FAILED,
                        serde_json::json!({
                            "seq": seq,
                            "failed": self.journal_write_failed,
                            "error": format!("{e:#}"),
                        }),
                    );
                }
            }
        }
    }

    fn audit(&self, kind: &str, mut detail: serde_json::Value) {
        if let Some(obj) = detail.as_object_mut() {
            obj.insert(
                "ts".into(),
                chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    .into(),
            );
            obj.insert("workspace".into(), self.cfg.workspace.clone().into());
            obj.insert("kind".into(), kind.into());
        }
        let _ = self.store.append_audit(&self.cfg.workspace, detail);
    }

    /// Remember one handled watch id, evicting oldest-first past the cap.
    fn remember_watch_id(&mut self, id: String) {
        const WATCH_SEEN_CAP: usize = 512;
        if self.watch_seen_set.insert(id.clone()) {
            self.watch_seen.push_back(id);
        }
        while self.watch_seen.len() > WATCH_SEEN_CAP {
            if let Some(old) = self.watch_seen.pop_front() {
                self.watch_seen_set.remove(&old);
            }
        }
    }

    fn persist_hear(&self) {
        self.store
            .save_unanswered(&self.cfg.workspace, &self.hear.unanswered_pins());
    }

    fn persist_wake(&mut self, woke: Option<&str>, ctx: &str) {
        let mut changed = false;
        if let Some(pin) = woke {
            changed |= doorbell_core::note_wake(&mut self.wake, pin, self.host.now_unix());
        }
        if changed {
            self.flush_wake(ctx);
        }
    }

    /// bd-jcys: the D4.2 write-ahead flush, named on failure.
    ///
    /// The invariant is decided-before-delivery: the decided row must be
    /// durable before a Prompt intent reaches a sink. When the flush fails the
    /// row is written but not on disk, and the prompt still goes out — that is
    /// deliberate. Withholding it would LOSE the wake: `on_watch` has already
    /// advanced and persisted the plane-S cursor, so the next ring dedups the
    /// event away and the prompt is gone for good. At-least-once delivery is
    /// the contract, so a delivered prompt with an undurable row is the smaller
    /// loss — and naming it is what keeps "the journal is the truth" honest.
    fn flush_journal(&mut self, ctx: &str) {
        let Err(e) = self.store.sync_journal(&self.cfg.workspace) else {
            return;
        };
        self.journal_sync_failed += 1;
        eprintln!(
            "doorbell-rt: journal write-ahead flush FAILED on {} ({ctx}) — refuse silent \
             drop (GS-039), the decided row may not be durable: {e:#}",
            self.cfg.workspace
        );
        if should_audit_count(self.journal_sync_failed as usize - 1) {
            self.audit(
                AUDIT_JOURNAL_SYNC_FAILED,
                serde_json::json!({
                    "context": ctx,
                    "failed": self.journal_sync_failed,
                    "error": format!("{e:#}"),
                }),
            );
        }
    }

    /// bd-jcys: a wake-state write that failed. Both consumers already fail
    /// SAFE — a lost plane-S cursor re-rings old history, a lost inbox
    /// high-water re-fetches — so this names the failure instead of changing
    /// the flow. `persist_wake` warned on its own path; the direct `save_wake`
    /// sites discarded the error entirely.
    fn flush_wake(&mut self, ctx: &str) {
        let Err(e) = self.store.save_wake(&self.cfg.workspace, &self.wake) else {
            return;
        };
        self.wake_save_failed += 1;
        eprintln!(
            "doorbell-rt: wake state write FAILED on {} ({ctx}) — refuse silent drop \
             (GS-039): {e:#}",
            self.cfg.workspace
        );
        if should_audit_count(self.wake_save_failed as usize - 1) {
            self.audit(
                AUDIT_WAKE_SAVE_FAILED,
                serde_json::json!({
                    "context": ctx,
                    "failed": self.wake_save_failed,
                    "error": format!("{e:#}"),
                }),
            );
        }
    }

    fn prompts_of(&self, effects: &[Effect]) -> Vec<(String, String)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Prompt { pin, line, .. } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect()
    }

    fn pins_of(&self, effects: &[Effect]) -> Vec<String> {
        self.prompts_of(effects)
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }

    fn snapshot_stats(&self) -> Stats {
        let mut s = self.stats.clone();
        if let Some(last) = self.sweep_last {
            s.sweep_lag_s = self.host.now_unix().saturating_sub(last);
        }
        s.deaf = self.hear.unanswered_pins().len() as u64;
        s.journal_rows = self.store.journal_row_count(&self.cfg.workspace);
        s
    }
}

/// rv23 #3: does this tick decide anything worth a journal row? The inbox
/// fetch request alone is the sweep's clock, not a decision: it re-enters as
/// `InboxFetched`, which journals when rows came back.
fn is_decision(effects: &[Effect]) -> bool {
    effects.iter().any(|e| !matches!(e, Effect::FetchInbox))
}

/// A stable fingerprint of one fetched row set (bd-0uzrq.20), so a parked pin
/// that keeps seeing the same rows reports them once instead of once per
/// backoff window.
fn inbox_rows_digest(rows: &[serde_json::Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for row in rows {
        row.to_string().hash(&mut hasher);
    }
    hasher.finish()
}

/// Journaled Prompt intents awaiting confirmation (crash-resume redelivery
/// set). Counts `{"Prompt": …}` actions across the loud fold.
fn count_journaled_prompts(store: &FileStore, workspace: &str) -> usize {
    store
        .fold_workspace(workspace)
        .rows
        .iter()
        .filter_map(|r| r.get("actions")?.as_array())
        .flatten()
        .filter(|a| a.get("Prompt").is_some())
        .count()
}

/// bd-zzgb: bound on one dispatch before the Brain warns it is holding
/// the message loop. A dispatch outliving this cannot see the Shutdown
/// queued behind it, so the Brain misses the 10s SIGTERM drain — naming
/// the variant HERE, while the process still lives, is the live pin.
const SLOW_DISPATCH_WARN: std::time::Duration = std::time::Duration::from_secs(2);

/// Variant name for the slow-dispatch warn. Exhaustive on purpose: a new
/// `BrainMsg` variant fails the build here instead of silently losing the
/// diagnostic.
fn brain_msg_label(msg: &BrainMsg) -> &'static str {
    match msg {
        BrainMsg::Watch(_) => "watch",
        BrainMsg::PlaneStatus { .. } => "status",
        BrainMsg::Died => "died",
        BrainMsg::Sweep { .. } => "sweep",
        BrainMsg::RoleBound { .. } => "rolebound",
        BrainMsg::Fact { .. } => "fact",
        BrainMsg::AckSlaTick { .. } => "ack-sla",
        BrainMsg::Presence { .. } => "presence",
        BrainMsg::InboxFetched { .. } => "inbox-fetched",
        BrainMsg::SlaFired { .. } => "sla-fired",
        BrainMsg::Degraded { .. } => "degraded",
        BrainMsg::Delivered { .. } => "delivered",
        BrainMsg::PromptRefused { .. } => "prompt-refused",
        BrainMsg::EventsHint { .. } => "events-hint",
        BrainMsg::EventsGap { .. } => "events-gap",
        BrainMsg::Ask { .. } => "ask",
        BrainMsg::Shutdown => "shutdown",
    }
}

/// Warns (via `on_slow`) when `work` outlives `SLOW_DISPATCH_WARN`, then
/// STILL awaits it to completion — never cancels (effect law: a mid-write
/// abort corrupts; the drain budget is what exits the process anyway). The
/// raced timer is what lets the warn print while a hung dispatch is still
/// running, i.e. before the drain deadline kills the daemon.
async fn run_dispatch_with_slow_warn<F>(label: &str, work: F, on_slow: impl FnOnce(&str))
where
    F: std::future::Future<Output = ()>,
{
    let mut work = std::pin::pin!(work);
    tokio::select! {
        _ = work.as_mut() => {}
        _ = tokio::time::sleep(SLOW_DISPATCH_WARN) => {
            on_slow(label);
            let _ = work.as_mut().await;
        }
    }
}

/// bd-burk: typed steps of the post-shutdown exit path. Each logs ON ENTRY,
/// so when a Brain never joins, doorbell.log's last line for it IS the step
/// holding it. Typed so a new step is spelled through the enum — no bare
/// literals at call sites — and the labels stay grep-stable for operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStep {
    ShutdownReceived,
    FlushDigest,
    SinkShutdown,
    JournalUndelivered,
    Complete,
}

impl ExitStep {
    fn as_str(self) -> &'static str {
        match self {
            ExitStep::ShutdownReceived => "shutdown received",
            ExitStep::FlushDigest => "flush digest",
            ExitStep::SinkShutdown => "sink shutdown",
            ExitStep::JournalUndelivered => "journal undelivered",
            ExitStep::Complete => "exit complete",
        }
    }

    /// One log line: step, workspace, dispatch backlog cleared, and elapsed
    /// since `Brain::run` started — the backlog discriminator (a Brain that
    /// logs `shutdown received` with a huge count ate its drain window
    /// clearing a deep queue; one that never logs it was parked).
    fn note(self, ws: &str, dispatched: usize, since_ms: u128) -> String {
        format!(
            "doorbell serve: brain {ws} exit: {} (dispatched={dispatched} +{since_ms}ms)",
            self.as_str()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockHost;
    use crate::sinks::MockSink;
    use std::collections::VecDeque;

    /// Death on the bell restarts before a watch that was already queued.
    /// If Died were `blocking_send`'d onto that channel, the watch would tick first.
    #[tokio::test]
    async fn death_bell_restarts_while_a_watch_is_still_queued() {
        let dir = std::env::temp_dir().join(format!("died-bell-brain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(BrainMsg::Watch("still-queued".into())).unwrap();
        let bell = crate::msg::DiedBell::new();
        bell.ring();
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let host = BellOrderHost {
            order: order.clone(),
        };
        let sinks = crate::sinks::SinkManager::new(tx.clone());
        let brain = Brain::new(
            BrainConfig::new("bell"),
            host,
            crate::store::FileStore::new(&dir),
            sinks,
        )
        .with_died_bell(bell);
        let run = tokio::spawn(brain.run(rx));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = tx.send(BrainMsg::Shutdown).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), run).await;
        let got = order.lock().expect("order").clone();
        assert_eq!(got.first().map(String::as_str), Some("died"), "{got:?}");
        let restart_at = got
            .iter()
            .position(|s| s == "restart")
            .unwrap_or_else(|| panic!("{got:?}"));
        if let Some(watch_at) = got.iter().position(|s| s == "watch") {
            assert!(restart_at < watch_at, "{got:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-jhz8x.2 arm (a)+(c): a blocked pane past the window reaches the
    /// CAPTAIN as one observe line carrying its lesson, once per episode; a
    /// resolved block re-rings on a recurrence. This is what replaces the
    /// `wave-observe` seat.
    #[tokio::test]
    async fn a_blocked_pane_escalates_to_the_captain_once_per_episode() {
        struct ObserveHost {
            captain: Option<String>,
        }
        impl Host for ObserveHost {
            fn tick(&mut self, _msg: &BrainMsg) -> Vec<crate::host::Effect> {
                Vec::new()
            }
            fn now_unix(&self) -> u64 {
                1_000
            }
            fn pins(&self) -> Vec<String> {
                vec!["CyanCove".to_string()]
            }
            fn captain_pin(&self) -> Option<String> {
                self.captain.clone()
            }
            fn fetch_inbox(&self, _pin: &str, _since: &str) -> Vec<serde_json::Value> {
                Vec::new()
            }
            fn scan_ack_sla(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn restart_wake(&mut self) -> bool {
                false
            }
        }
        let dir = std::env::temp_dir().join(format!("observe-brain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let sinks = SinkManager::new(tx.clone());
        let (sink, sent) = MockSink::new();
        let mut brain = Brain::new(
            BrainConfig::new("observe"),
            ObserveHost {
                captain: Some("SlateHarbor".to_string()),
            },
            FileStore::new(dir.clone()),
            sinks,
        );
        brain.sinks.add_sink("hc", Box::new(sink));

        // Blocked at t=1000; the window is 600s.
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "CyanCove".into(),
                status: "blocked".into(),
            })
            .await;
        // The sink worker is its own task, so "nothing sent" needs a settle
        // window and "sent" needs a wait — never a bare len() right after.
        async fn settle() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        async fn wait_for(
            sent: &std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
            want: usize,
        ) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if sent.lock().unwrap_or_else(|e| e.into_inner()).len() >= want {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("expected {want} escalation line(s)"));
        }

        brain.dispatch(BrainMsg::Sweep { now: 1_599 }).await;
        settle().await;
        assert!(
            sent.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "under the window nothing escalates"
        );
        brain.dispatch(BrainMsg::Sweep { now: 1_600 }).await;
        wait_for(&sent, 1).await;
        let lines: Vec<String> = sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(pin, line)| format!("{pin}|{line}"))
            .collect();
        assert_eq!(lines.len(), 1, "one escalation: {lines:?}");
        assert!(lines[0].starts_with("SlateHarbor|"), "{lines:?}");
        assert!(lines[0].contains("[observe] blocked-bead"), "{lines:?}");
        assert!(
            lines[0].contains("lesson:"),
            "the lesson rides along: {lines:?}"
        );

        // Same episode: silent on the next sweep.
        brain.dispatch(BrainMsg::Sweep { now: 1_900 }).await;
        settle().await;
        assert_eq!(
            sent.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "one ring per episode"
        );

        // Resolved, then blocked again: a new episode rings again.
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "CyanCove".into(),
                status: "idle".into(),
            })
            .await;
        brain.dispatch(BrainMsg::Sweep { now: 2_000 }).await;
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "CyanCove".into(),
                status: "blocked".into(),
            })
            .await;
        brain.dispatch(BrainMsg::Sweep { now: 2_601 }).await;
        wait_for(&sent, 2).await;
        assert_eq!(
            sent.lock().unwrap_or_else(|e| e.into_inner()).len(),
            2,
            "a recurrence is a new episode"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-jhz8x.2 arm (c): a `[fork] <bead>: <question>` SUBJECT reaches the
    /// captain once per bead, with the question as evidence. Two forks on one
    /// bead are still one episode; a normal subject is never a fork.
    #[tokio::test]
    async fn a_fork_subject_escalates_to_the_captain_once_per_bead() {
        struct ForkHost;
        impl Host for ForkHost {
            fn tick(&mut self, _msg: &BrainMsg) -> Vec<crate::host::Effect> {
                Vec::new()
            }
            fn now_unix(&self) -> u64 {
                1_000
            }
            fn pins(&self) -> Vec<String> {
                vec!["CyanCove".to_string()]
            }
            fn captain_pin(&self) -> Option<String> {
                Some("SlateHarbor".to_string())
            }
            fn fetch_inbox(&self, _pin: &str, _since: &str) -> Vec<serde_json::Value> {
                Vec::new()
            }
            fn scan_ack_sla(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn restart_wake(&mut self) -> bool {
                false
            }
        }
        let dir = std::env::temp_dir().join(format!("fork-brain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let sinks = SinkManager::new(tx.clone());
        let (sink, sent) = MockSink::new();
        let mut brain = Brain::new(
            BrainConfig::new("fork"),
            ForkHost,
            FileStore::new(dir.clone()),
            sinks,
        );
        brain.sinks.add_sink("hc", Box::new(sink));

        brain
            .dispatch(BrainMsg::InboxFetched {
                pin: "CyanCove".into(),
                rows: vec![
                    serde_json::json!({"subject": "[fork] bd-a: fix here or hand over?"}),
                    serde_json::json!({"subject": "[fork] bd-a: or split the bead?"}),
                    serde_json::json!({"subject": "bd-b: not a fork"}),
                ],
            })
            .await;
        brain.dispatch(BrainMsg::Sweep { now: 1_100 }).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !sent.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the fork reaches the captain");
        let lines: Vec<String> = sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(pin, line)| format!("{pin}|{line}"))
            .collect();
        assert_eq!(lines.len(), 1, "one episode per bead: {lines:?}");
        assert!(
            lines[0].starts_with("SlateHarbor|[observe] fork bd-a:"),
            "{lines:?}"
        );
        assert!(lines[0].contains("fix here or hand over?"), "{lines:?}");
        assert!(lines[0].contains("lesson:"), "{lines:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-jhz8x.2 arm (b): a request-release the holder never answered inside
    /// one lease period reaches the captain once, with holder, bead and age;
    /// the response clears it, and a later ask is a fresh episode.
    #[tokio::test]
    async fn an_unanswered_release_request_escalates_to_the_captain_once() {
        struct ReleaseHost;
        impl Host for ReleaseHost {
            fn tick(&mut self, _msg: &BrainMsg) -> Vec<crate::host::Effect> {
                Vec::new()
            }
            fn now_unix(&self) -> u64 {
                1_000
            }
            fn pins(&self) -> Vec<String> {
                vec!["CyanCove".to_string()]
            }
            fn captain_pin(&self) -> Option<String> {
                Some("SlateHarbor".to_string())
            }
            fn fetch_inbox(&self, _pin: &str, _since: &str) -> Vec<serde_json::Value> {
                Vec::new()
            }
            fn scan_ack_sla(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn restart_wake(&mut self) -> bool {
                false
            }
        }
        let dir = std::env::temp_dir().join(format!("release-brain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let sinks = SinkManager::new(tx.clone());
        let (sink, sent) = MockSink::new();
        let mut brain = Brain::new(
            BrainConfig::new("release"),
            ReleaseHost,
            FileStore::new(dir.clone()),
            sinks,
        );
        brain.sinks.add_sink("hc", Box::new(sink));

        // asked at t=2000s by BoldBay, held by CyanCove, on bd-a.
        let request = serde_json::json!({
            "subject": "p2p:request-release src/a.rs",
            "sender": "BoldBay",
            "created_ts_us": 2_000_000_000i64,
            "body": r#"{"verb":"request-release","path":"src/a.rs","bead":"bd-a"}"#,
        });
        brain
            .dispatch(BrainMsg::InboxFetched {
                pin: "CyanCove".into(),
                rows: vec![request.clone()],
            })
            .await;
        brain.dispatch(BrainMsg::Sweep { now: 2_179 }).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            sent.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "under one lease period it is just a request"
        );
        brain.dispatch(BrainMsg::Sweep { now: 2_180 }).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !sent.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the unanswered ask reaches the captain");
        let lines: Vec<String> = sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(pin, line)| format!("{pin}|{line}"))
            .collect();
        assert_eq!(lines.len(), 1, "one ring per episode: {lines:?}");
        assert!(
            lines[0].starts_with("SlateHarbor|[observe] reservation-ladder src/a.rs:"),
            "{lines:?}"
        );
        for field in ["CyanCove", "bd-a", "180s", "BoldBay"] {
            assert!(lines[0].contains(field), "missing {field}: {lines:?}");
        }

        // Answered: the ask is cleared, never repeats.
        brain
            .dispatch(BrainMsg::InboxFetched {
                pin: "BoldBay".into(),
                rows: vec![serde_json::json!({
                    "subject": "p2p:release-response src/a.rs",
                    "sender": "CyanCove",
                })],
            })
            .await;
        brain.dispatch(BrainMsg::Sweep { now: 2_300 }).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            sent.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "an answered ask is silent"
        );

        // A LATER ask on the same path is a new episode.
        brain
            .dispatch(BrainMsg::InboxFetched {
                pin: "CyanCove".into(),
                rows: vec![serde_json::json!({
                    "subject": "p2p:request-release src/a.rs",
                    "sender": "BoldBay",
                    "created_ts_us": 2_400_000_000i64,
                    "body": r#"{"verb":"request-release","path":"src/a.rs","bead":"bd-a"}"#,
                })],
            })
            .await;
        brain.dispatch(BrainMsg::Sweep { now: 2_580 }).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if sent.lock().unwrap_or_else(|e| e.into_inner()).len() >= 2 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a recurrence rings again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-1i8c: the brain must hand the inbox fetch to the host's OFF-LOOP
    /// lane, never fetch inline.
    ///
    /// The brain is a SINGLE-SERIAL writer: an inline fetch parked it for
    /// the whole scan (one CLI per pin, each with its own ceiling), so the
    /// wake channel filled and real wake lines were DROPPED (live: 196,167
    /// forwarded, 27,791 brain_full, 1,879 dropped). The rows re-enter as
    /// `InboxFetched` messages instead.
    ///
    /// The host's inline `fetch_inbox` and `scan_ack_sla` PANIC here: an
    /// inline fetch must fail this test, not merely change its output.
    #[tokio::test]
    async fn brain_hands_the_inbox_fetch_off_the_dispatch() {
        struct FetchHost {
            handed: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
            scanned: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl Host for FetchHost {
            fn tick(&mut self, _msg: &BrainMsg) -> Vec<crate::host::Effect> {
                vec![crate::host::Effect::FetchInbox]
            }
            fn now_unix(&self) -> u64 {
                1
            }
            fn fetch_inbox(&self, _pin: &str, _since: &str) -> Vec<serde_json::Value> {
                panic!("bd-1i8c: the brain must not fetch inline");
            }
            fn scan_ack_sla(&mut self) -> Vec<String> {
                panic!("bd-1i8c: the brain must not scan the SLA inline");
            }
            fn fetch_inbox_detached(
                &mut self,
                pins: Vec<(String, String)>,
            ) -> Vec<(String, Vec<serde_json::Value>)> {
                self.handed.lock().expect("handed").extend(pins);
                Vec::new()
            }
            fn scan_ack_sla_detached(&mut self) -> Vec<String> {
                self.scanned
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Vec::new()
            }
            fn restart_wake(&mut self) -> bool {
                false
            }
            fn pins(&self) -> Vec<String> {
                vec!["AmberOwl".to_string()]
            }
        }
        let handed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let scanned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let host = FetchHost {
            handed: handed.clone(),
            scanned: scanned.clone(),
        };
        let dir = std::env::temp_dir().join(format!("dbrt-1i8c-brain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let mut brain = Brain::new(
            BrainConfig::new("dbrt-1i8c"),
            host,
            crate::store::FileStore::new(&dir),
            crate::sinks::SinkManager::new(tx),
        );
        brain.dispatch(BrainMsg::Sweep { now: 1 }).await;
        let handed = handed.lock().expect("handed").clone();
        assert_eq!(
            handed.len(),
            1,
            "the sweep must hand the (pin, since) pair to the off-loop lane: {handed:?}"
        );
        assert_eq!(handed[0].0, "AmberOwl", "the pin is the sweep's pin");
        assert!(
            !handed[0].1.is_empty(),
            "the hand-off carries the cursor, not an empty window"
        );
        assert!(
            !scanned.load(std::sync::atomic::Ordering::SeqCst),
            "a sweep does not scan the SLA"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct BellOrderHost {
        order: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Host for BellOrderHost {
        fn tick(&mut self, msg: &BrainMsg) -> Vec<crate::host::Effect> {
            let label = match msg {
                BrainMsg::Died => "died",
                BrainMsg::Watch(_) => "watch",
                _ => "other",
            };
            self.order.lock().expect("order").push(label.to_string());
            Vec::new()
        }
        fn now_unix(&self) -> u64 {
            1
        }
        fn fetch_inbox(&self, _: &str, _: &str) -> Vec<serde_json::Value> {
            Vec::new()
        }
        fn scan_ack_sla(&mut self) -> Vec<String> {
            Vec::new()
        }
        /// bd-1i8c: the order host has no mailbox — the default (inline)
        /// fetch is the honest shape for it.
        fn fetch_inbox_detached(
            &mut self,
            _pins: Vec<(String, String)>,
        ) -> Vec<(String, Vec<serde_json::Value>)> {
            Vec::new()
        }
        /// bd-1i8c: same — no SLA set to scan.
        fn scan_ack_sla_detached(&mut self) -> Vec<String> {
            Vec::new()
        }
        fn restart_wake(&mut self) -> bool {
            self.order.lock().expect("order").push("restart".into());
            true
        }
    }

    /// bd-y40k: the three-way straggler diagnosis, pure. Starvation wins on
    /// age (the bead's rule: stale = starved); growth is only consulted
    /// while the wake is recent, because a moving age already answers.
    #[test]
    fn liveness_classifies_starved_churning_lost_wake() {
        use super::{LivenessVerdict, STARVATION_THRESHOLD_MS, classify_liveness};
        assert!(
            matches!(
                classify_liveness(STARVATION_THRESHOLD_MS, false),
                LivenessVerdict::Starved { .. }
            ),
            "age at threshold is starved"
        );
        assert!(
            matches!(
                classify_liveness(10_200, false),
                LivenessVerdict::Starved { .. }
            ),
            "a whole budget without a wake is starved"
        );
        assert!(
            matches!(
                classify_liveness(80, true),
                LivenessVerdict::Churning { .. }
            ),
            "recent wake + growing count = backlog churn"
        );
        assert!(
            matches!(
                classify_liveness(80, false),
                LivenessVerdict::LostWake { .. }
            ),
            "recent wake + flat count across the probe = parked after one wake"
        );
        assert!(
            LivenessVerdict::LostWake { age_ms: 80 }
                .describe()
                .contains("parked, mid-exit-path, or mid-dispatch"),
            "LostWake names all three candidates: {}",
            LivenessVerdict::LostWake { age_ms: 80 }.describe()
        );
        assert!(
            LivenessVerdict::Starved { age_ms: 10_200 }
                .describe()
                .contains("dispatch"),
            "Starved points at the slow-dispatch-warn cross-check: {}",
            LivenessVerdict::Starved { age_ms: 10_200 }.describe()
        );
    }

    /// The probe follows the substituted clock: touch stamps virtual now,
    /// sleeps advance it, and age reads the gap since the last touch. A
    /// brain that never touches again crosses the starvation threshold.
    #[tokio::test(start_paused = true)]
    async fn liveness_touch_tracks_wakes_and_age_on_the_clock() {
        use super::{BrainLiveness, STARVATION_THRESHOLD_MS, classify_liveness};
        let liv = BrainLiveness::new();
        liv.touch();
        tokio::time::sleep(std::time::Duration::from_millis(3_000)).await;
        liv.touch();
        let age = liv.age_ms(tokio::time::Instant::now());
        assert_eq!(liv.wakes(), 2, "one count per wake");
        assert!(age < 100, "just touched: age {age}ms");
        tokio::time::sleep(std::time::Duration::from_millis(5_000)).await;
        let age = liv.age_ms(tokio::time::Instant::now());
        assert!(
            age >= STARVATION_THRESHOLD_MS,
            "parked 5s: age {age}ms crosses the threshold"
        );
        assert!(
            matches!(
                classify_liveness(age, false),
                super::LivenessVerdict::Starved { .. }
            ),
            "parked classifies starved"
        );
    }

    /// A fresh probe starts at wake-zero-with-creation-baseline: age is 0
    /// right after construction/touch and never underflows.
    #[tokio::test(start_paused = true)]
    async fn liveness_fresh_probe_reads_age_zero() {
        let liv = super::BrainLiveness::new();
        liv.touch();
        assert_eq!(liv.age_ms(tokio::time::Instant::now()), 0);
        assert_eq!(liv.wakes(), 1);
    }

    fn test_brain(
        name: &str,
        script: Vec<Vec<Effect>>,
    ) -> (
        Brain<MockHost>,
        tokio::sync::mpsc::Receiver<BrainMsg>,
        tokio::sync::mpsc::Sender<BrainMsg>,
    ) {
        let dir = std::env::temp_dir().join(format!("dbrt-brain-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        let (tx, rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let mut host = MockHost::new(1_000_000);
        host.script = VecDeque::from(script);
        let sinks = SinkManager::new(tx.clone());
        let brain = Brain::new(BrainConfig::new("w"), host, FileStore::new(dir), sinks);
        (brain, rx, tx)
    }

    fn journal_kinds(name: &str) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("dbrt-brain-{}-{}", std::process::id(), name));
        let raw =
            std::fs::read_to_string(FileStore::new(&dir).journal_file("w")).unwrap_or_default();
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v["msg"]["rt_kind"].as_str().map(str::to_string))
            .collect()
    }

    /// bd-0uzrq.20: an unroutable pin's rows used to hold the cursor, so every
    /// sweep refetched them — live: one toron round trip every ~3s for the
    /// captain's pin, each returning rows nobody could route. The pin is parked
    /// now: 60 sweeps at 3s across 180s fetch it once per backoff window, not
    /// 60 times, and the same row set journals ONE typed unroutable row.
    #[tokio::test]
    async fn bd20_unroutable_pin_is_fetched_once_per_backoff_window() {
        let name = "bd20-unroutable-backoff";
        let script: Vec<Vec<Effect>> = (0..200).map(|_| vec![Effect::FetchInbox]).collect();
        let (mut brain, _rx, _tx) = test_brain(name, script);
        brain.host.pin_list = vec!["SlateHarbor".to_string()];
        brain.host.unroutable.insert("SlateHarbor".to_string());
        // A wake the fold never decides, and no timestamp to advance a cursor
        // to: the live shape that refetched the same rows forever.
        let rows = vec![serde_json::json!({
            "id": "m1",
            "subject": "brief from the human",
            "sender": "SlateHarbor",
        })];
        let t0 = 1_000_000u64;
        for step in [0u64, 20, 40] {
            let at = t0 + step * 3;
            let floor = doorbell_core::redelivery_floor_rfc3339(at);
            brain
                .host
                .inbox
                .insert(("SlateHarbor".to_string(), floor), rows.clone());
        }
        for step in 0..60u64 {
            brain.dispatch(BrainMsg::Sweep { now: t0 + step * 3 }).await;
        }
        let fetches = brain
            .host
            .fetch_calls
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            fetches, 3,
            "180s of sweeps at a 60s backoff is three probes, not sixty"
        );
        let dir = std::env::temp_dir().join(format!("dbrt-brain-{}-{}", std::process::id(), name));
        let raw =
            std::fs::read_to_string(FileStore::new(&dir).journal_file("w")).unwrap_or_default();
        assert_eq!(
            raw.lines()
                .filter(|line| line.contains("\"unroutable\""))
                .count(),
            1,
            "the same row set is reported once, not once per window: {raw}"
        );
    }

    /// The backoff is for pins with NO route only. A routable pin must keep the
    /// every-sweep fetch that rings its mail — parking it would starve a real
    /// crew wake.
    #[tokio::test]
    async fn bd20_routable_pin_still_fetches_every_sweep() {
        let name = "bd20-routable-every-sweep";
        let script: Vec<Vec<Effect>> = (0..10).map(|_| vec![Effect::FetchInbox]).collect();
        let (mut brain, _rx, _tx) = test_brain(name, script);
        brain.host.pin_list = vec!["PinA".to_string()];
        for step in 0..10u64 {
            brain
                .dispatch(BrainMsg::Sweep {
                    now: 1_000_000 + step * 3,
                })
                .await;
        }
        assert_eq!(
            brain
                .host
                .fetch_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            10,
            "a routable pin is polled every sweep, backoff or not"
        );
    }

    /// The other half of the contract (bd-0uzrq.20): a ROUTABLE pin whose rows
    /// the fold does not decide keeps its cursor, so the next sweep re-reads
    /// them. That hold is how a wake survives until the pane can take it, and
    /// it must never be parked like an unroutable pin.
    #[tokio::test]
    async fn bd20_routable_pin_keeps_its_cursor_on_undecided_rows() {
        let name = "bd20-routable-hold";
        let script: Vec<Vec<Effect>> = (0..4).map(|_| vec![Effect::FetchInbox]).collect();
        let (mut brain, _rx, _tx) = test_brain(name, script);
        brain.host.pin_list = vec!["PinA".to_string()];
        let rows = vec![serde_json::json!({
            "id": "m1",
            "subject": "brief from a peer",
            "sender": "Peer",
            "created_ts_us": 1_000_000_500i64,
        })];
        let t0 = 1_000_000u64;
        for step in [0u64, 1] {
            let floor = doorbell_core::redelivery_floor_rfc3339(t0 + step);
            brain
                .host
                .inbox
                .insert(("PinA".to_string(), floor), rows.clone());
        }
        brain.dispatch(BrainMsg::Sweep { now: t0 }).await;
        brain.dispatch(BrainMsg::Sweep { now: t0 + 1 }).await;
        assert!(
            !brain.wake.since_by_pin.contains_key("PinA"),
            "a routable pin keeps its cursor while its rows are undecided"
        );
        assert_eq!(
            brain
                .host
                .fetch_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "and it is re-read on every sweep, not parked"
        );
    }

    /// bd-0uzrq.17: the status lane is polled BEFORE mail. With one channel a
    /// status queued behind a backlog of mail-bound dispatches reached the fold
    /// 76-155s late (live 16:16-16:19Z) and the idle edge it carried drained
    /// nothing. A `Shutdown` already queued on the mail lane is the sharpest
    /// form of that backlog: FIFO would end the loop before the status is ever
    /// seen, and the biased lane must journal the status instead.
    #[tokio::test]
    async fn a_status_lane_overtakes_mail_already_queued() {
        let (brain, rx, tx) = test_brain("lane-order", vec![]);
        let (status_tx, status_rx) = tokio::sync::mpsc::channel(4);
        tx.send(BrainMsg::Shutdown).await.expect("mail lane");
        status_tx
            .send(BrainMsg::PlaneStatus {
                pin: "PinA".into(),
                status: "idle".into(),
            })
            .await
            .expect("status lane");
        brain.run_with_status(rx, Some(status_rx)).await;
        let kinds = journal_kinds("lane-order");
        assert!(
            kinds.iter().any(|k| k == "PlaneStatus"),
            "the status overtook the queued shutdown: {kinds:?}"
        );
    }

    /// A ring loaded from disk has no pending entry, so only the pin→npub
    /// map can clear it when the agent replies. Live: SwiftBay answered and
    /// the status still reported it unanswered until the host index filled
    /// that map at start.
    #[tokio::test]
    async fn a_seeded_unanswered_pin_clears_when_its_known_npub_is_heard() {
        let dir = std::env::temp_dir().join(format!("dbrt-brain-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileStore::new(&dir);
        store.save_unanswered("w", &["SwiftBay".to_string()]);
        let mut host = MockHost::new(1_000_000);
        host.npubs.insert("SwiftBay".into(), "ab12".into());
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let sinks = SinkManager::new(tx);
        let mut brain = Brain::new(BrainConfig::new("w"), host, store, sinks);
        assert_eq!(brain.hear.unanswered_pins(), vec!["SwiftBay".to_string()]);
        brain.hear.note_heard_pubkey("AB12");
        assert!(
            brain.hear.unanswered_pins().is_empty(),
            "the seeded ring must carry the host-known npub"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The post-start gap: the host index learns the npub AFTER `Brain::new`
    /// (registry refresh on the 1s tick), so the seeded pin's map entry is
    /// empty until a sweep refreshes it. Without the refresh the late reply
    /// never clears the pin — it only expires at `UNANSWERED_SECS`.
    #[tokio::test]
    async fn sweep_refreshes_a_late_learned_npub_so_the_seeded_pin_clears_on_reply() {
        let dir = std::env::temp_dir().join(format!("dbrt-brain-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileStore::new(&dir);
        store.save_unanswered("w", &["LatePin".to_string()]);
        let host = MockHost::new(1_000_000);
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let sinks = SinkManager::new(tx);
        let mut brain = Brain::new(BrainConfig::new("w"), host, store, sinks);
        assert_eq!(brain.hear.unanswered_pins(), vec!["LatePin".to_string()]);
        // The host index learns the npub after start.
        brain.host.npubs.insert("LatePin".into(), "cd34".into());
        // An unknown npub keeps today's behavior: the pin stays.
        brain.hear.note_heard_pubkey("zzzz");
        assert_eq!(brain.hear.unanswered_pins(), vec!["LatePin".to_string()]);
        brain.dispatch(BrainMsg::Sweep { now: 1_000_001 }).await;
        brain.hear.note_heard_pubkey("CD34");
        assert!(
            brain.hear.unanswered_pins().is_empty(),
            "the sweep must refresh the pin→npub map for unanswered pins"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rv23 #3: an idle sweep decides nothing, so it writes no journal row
    /// (it was ~1 row/s, and the journal is what `/health` folds). The beat
    /// still lands, in the sweep stamp. A sweep that DOES decide journals.
    #[tokio::test]
    async fn an_idle_sweep_writes_no_journal_row_but_stamps_the_beat() {
        let name = "rv23-idle-sweep";
        let audit = Effect::Audit {
            kind: "k".into(),
            pin: "p".into(),
            detail: serde_json::json!({}),
        };
        let (mut brain, _rx, _tx) = test_brain(
            name,
            vec![
                vec![Effect::FetchInbox],
                vec![],
                vec![Effect::FetchInbox, audit],
            ],
        );
        brain.dispatch(BrainMsg::Sweep { now: 1_000_001 }).await;
        brain.dispatch(BrainMsg::Sweep { now: 1_000_002 }).await;
        assert!(
            journal_kinds(name).is_empty(),
            "two no-op sweeps, no rows: {:?}",
            journal_kinds(name)
        );
        let dir = std::env::temp_dir().join(format!("dbrt-brain-{}-{}", std::process::id(), name));
        let stamp = FileStore::new(&dir).sweep_stamp_file("w");
        assert_eq!(
            std::fs::read_to_string(&stamp).unwrap().trim(),
            "1000002",
            "the beat is stamped every sweep"
        );
        brain.dispatch(BrainMsg::Sweep { now: 1_000_003 }).await;
        assert_eq!(journal_kinds(name), vec!["Sweep".to_string()]);
    }

    /// rv23 #3: an empty inbox fetch that decides nothing is not a row; a
    /// fetch that brought rows back is.
    #[tokio::test]
    async fn an_empty_inbox_fetch_writes_no_journal_row() {
        let name = "rv23-empty-fetch";
        let (mut brain, _rx, _tx) = test_brain(name, vec![]);
        brain
            .dispatch(BrainMsg::InboxFetched {
                pin: "p".into(),
                rows: vec![],
            })
            .await;
        assert!(journal_kinds(name).is_empty(), "{:?}", journal_kinds(name));
        brain
            .dispatch(BrainMsg::InboxFetched {
                pin: "p".into(),
                rows: vec![serde_json::json!({"id": 1, "created_ts": "2026-10-23T00:00:00Z"})],
            })
            .await;
        assert_eq!(journal_kinds(name), vec!["InboxFetched".to_string()]);
    }

    /// bd-nuf2: Brain::run exits inside the drain budget with a hung
    /// endpoint — Shutdown breaks the loop, the digest flushes, the sink
    /// join gives up on the straggler, the drain journals, and the report
    /// returns. Multi-thread: the stall blocks a thread, like the live
    /// 30s CLI send.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn brain_run_exits_inside_budget_with_hung_endpoint() {
        let (mut brain, rx, tx) = test_brain("drain", vec![]);
        let (mut sink, _sent) = MockSink::new();
        sink.stall_ms = 12_000;
        brain.sinks.add_sink("hc", Box::new(sink));
        brain.sinks.send("hc", "p", "slow").await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        tx.send(BrainMsg::Shutdown).await.expect("shutdown queues");
        let report = tokio::time::timeout(std::time::Duration::from_secs(8), brain.run(rx))
            .await
            .expect("run must exit inside budget with a hung endpoint");
        let _ = report;
    }

    /// bd-3mrc: graceful shutdown journals the drain AND boot still resumes.
    /// Crash-resume is pinned by crash_abort_restart_...; this pins the drain
    /// path end to end: endpoint down parks the sink, queued intents drain into
    /// the journaled drain row, reboot on the same store refolds budgets and
    /// logs the resume row. If the drain row ever stops riding journal() or
    /// pre-delivery journaling regresses, this goes red while crash stays green.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_boot_still_resumes_drained_prompts() {
        use std::collections::VecDeque;
        let dir = std::env::temp_dir().join(format!("dbrt-graceful-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mk = || {
            let (tx, rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
            let mut host = MockHost::new(1_000_000);
            host.script = VecDeque::from(vec![
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "m1".into(),
                }],
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "m2".into(),
                }],
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "m3".into(),
                }],
            ]);
            let sinks = SinkManager::new(tx.clone());
            let brain = Brain::new(BrainConfig::new("w"), host, FileStore::new(&dir), sinks);
            (brain, rx, tx)
        };
        let (mut a, rx, tx) = mk();
        let (mut sink, _sent) = MockSink::new();
        sink.fail_after = Some(0);
        a.sinks.add_sink("hc", Box::new(sink));
        tx.send(BrainMsg::Watch("{\"kind\":9}".into()))
            .await
            .expect("watch queues");
        tx.send(BrainMsg::Fact {
            summary: "x".into(),
        })
        .await
        .expect("fact queues");
        tx.send(BrainMsg::Fact {
            summary: "y".into(),
        })
        .await
        .expect("fact queues");
        tx.send(BrainMsg::Shutdown).await.expect("shutdown queues");
        let report = tokio::time::timeout(std::time::Duration::from_secs(8), a.run(rx))
            .await
            .expect("run must exit on shutdown");
        assert_eq!(
            report.undelivered.len(),
            1,
            "the first failure parks the sink and the rest shed to digest by design; the queued remainder must drain as undelivered"
        );
        let journal = std::fs::read_to_string(FileStore::new(&dir).journal_file("w")).unwrap();
        assert!(
            journal.contains("\"stage\":\"drain\"") && journal.contains("\"undelivered\":1"),
            "drain row journals the set: {journal}"
        );
        drop(report);
        let (b, _rx2, _tx2) = mk();
        let budget = b.budgets.get("p").expect("refolded budget row");
        assert_eq!(budget.prompts.len(), 3, "all three grants refolded");
        let journal = std::fs::read_to_string(FileStore::new(&dir).journal_file("w")).unwrap();
        assert!(
            journal.contains("\"stage\":\"resume\""),
            "boot logs its resume row: {journal}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-zzgb: a dispatch outliving the warn bound must be NAMED while it
    /// still runs (the drain prints this before the process exits) and must
    /// NOT be cancelled — a mid-write abort corrupts, and the budget is what
    /// exits us. The work still runs to completion after the warn.
    #[tokio::test(start_paused = true)]
    async fn slow_dispatch_warns_once_and_still_finishes() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        super::run_dispatch_with_slow_warn(
            "sweep",
            async { tokio::time::sleep(std::time::Duration::from_secs(30)).await },
            move |label| seen_c.lock().unwrap().push(label.to_string()),
        )
        .await;
        assert_eq!(seen.lock().unwrap().join(","), "sweep");
    }

    #[tokio::test(start_paused = true)]
    async fn fast_dispatch_never_warns() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        super::run_dispatch_with_slow_warn("fact", std::future::ready(()), move |label| {
            seen_c.lock().unwrap().push(label.to_string())
        })
        .await;
        assert!(seen.lock().unwrap().is_empty(), "fast dispatch stays quiet");
    }

    /// bd-burk: the exit-path note must name the step, the workspace, the
    /// dispatch backlog it cleared, and the elapsed time — that note IS the
    /// pin (doorbell.log's last line for a straggler = the step holding it).
    #[test]
    fn exit_step_note_names_the_blocking_step_with_backlog_and_time() {
        let note = super::ExitStep::FlushDigest.note("flywheel/flywheel-flywheel", 41, 12);
        assert!(note.contains("exit: flush digest"), "{note}");
        assert!(note.contains("flywheel/flywheel-flywheel"), "{note}");
        assert!(note.contains("dispatched=41"), "{note}");
        assert!(note.contains("+12ms"), "{note}");
    }

    /// The step strings are what operators grep for — pin them, and force
    /// every new exit step to be spelled through the enum (no bare literals
    /// at the call sites).
    #[test]
    fn exit_step_labels_are_stable() {
        use super::ExitStep::*;
        assert_eq!(ShutdownReceived.as_str(), "shutdown received");
        assert_eq!(FlushDigest.as_str(), "flush digest");
        assert_eq!(SinkShutdown.as_str(), "sink shutdown");
        assert_eq!(JournalUndelivered.as_str(), "journal undelivered");
        assert_eq!(Complete.as_str(), "exit complete");
    }

    /// The backlog discriminator: a Brain that logs "shutdown received" with
    /// a huge dispatched count ate its 10s clearing a deep queue (no single
    /// slow dispatch exists); one that NEVER logs it was parked pre-break
    /// (timer flush or undelivered Shutdown).
    #[test]
    fn shutdown_received_note_carries_the_dispatch_backlog() {
        let note = super::ExitStep::ShutdownReceived.note("flywheel/flywheel-flywheel", 41, 10_230);
        assert!(note.contains("shutdown received"), "{note}");
        assert!(note.contains("dispatched=41"), "{note}");
        assert!(note.contains("+10230ms"), "{note}");
    }

    /// bd-adms: `toron ack-sla` re-reports every corr still past SLA, so an
    /// unchanged backlog re-fired the same wake ~1440x/day (live: `count: 89`
    /// on every beat) — noise that buries a genuine regression. Only a CHANGE
    /// is news.
    /// bd-w98c residual: the SlaFired hit list is a real reducer Msg. When
    /// the tick answers FetchInbox, the brain pipes rows back as
    /// InboxFetched (same shape as the sweep/acksla live arms) — that
    /// re-entry is what registers overdue corrs on the ladder.
    #[tokio::test]
    async fn sla_fired_fetch_answer_pipes_rows_back() {
        let (mut brain, _rx, _tx) = test_brain(
            "sla-pipe",
            vec![
                vec![
                    Effect::Audit {
                        kind: "ack-sla-hit".into(),
                        pin: String::new(),
                        detail: serde_json::json!({ "overdue": ["c-1"] }),
                    },
                    Effect::FetchInbox,
                ],
                vec![],
            ],
        );
        brain.host.pin_list = vec!["p".into()];
        let floor = doorbell_core::redelivery_floor_rfc3339(brain.host.now);
        brain.host.inbox.insert(
            ("p".into(), floor),
            vec![serde_json::json!({
                "id": "m1", "corr": "c-1", "ack_required": true,
                "importance": "high", "read_ts_us": null, "created_ts": "2026-09-16T23:59:00.000000Z"
            })],
        );
        brain
            .dispatch(BrainMsg::SlaFired {
                posts: vec!["c-1".into()],
            })
            .await;
        assert_eq!(
            brain.host.tick_log.len(),
            2,
            "hit list must pipe the inbox fold, not stop at the audit: {:?}",
            brain.host.tick_log
        );
        assert!(
            brain.host.tick_log[1].starts_with("InboxFetched"),
            "rows re-enter as InboxFetched: {:?}",
            brain.host.tick_log
        );
    }

    #[tokio::test]
    async fn sla_refire_is_a_delta_not_a_repeat() {
        let (mut brain, _rx, _tx) = test_brain(
            "sla-delta",
            (0..8).map(|_| vec![Effect::CheckAckSla]).collect(),
        );
        let overdue = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        brain.host.sla = overdue(&["corr-1", "corr-2"]);
        brain.on_ack_sla(1_000).await;
        brain.host.sla = overdue(&["corr-1", "corr-2"]);
        brain.on_ack_sla(1_060).await;
        brain.host.sla = overdue(&["corr-1", "corr-2", "corr-3"]);
        brain.on_ack_sla(1_120).await;
        brain.host.sla = Vec::new();
        brain.on_ack_sla(1_180).await;

        let audit = std::fs::read_to_string(brain.store.audit_file("w")).unwrap();
        let fired = audit.matches("\"ack_sla_fired\"").count();
        assert_eq!(
            fired, 2,
            "two changes, two audits (not one per beat): {audit}"
        );
        assert!(
            audit.contains("\"ack_sla_cleared\""),
            "the backlog clearing is named once: {audit}"
        );
    }

    #[tokio::test]
    async fn decided_before_delivery_write_ahead() {
        let prompt = Effect::Prompt {
            event_id: None,
            pin: "p".into(),
            line: "[doorbell] test".into(),
        };
        let (mut brain, _rx, _tx) = test_brain("decided", vec![vec![prompt]]);
        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        assert_eq!(brain.stats.injects, 0, "no sink registered");
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert!(journal.contains("\"stage\":\"decided\""), "{journal}");
    }

    #[tokio::test]
    async fn watch_dedup_leaves_a_journal_trace() {
        let (mut brain, _rx, _tx) = test_brain("dedup", vec![vec![], vec![], vec![]]);
        brain
            .dispatch(BrainMsg::Watch("{\"kind\":9,\"created_at\":100}".into()))
            .await;
        for ts in [90, 95, 95] {
            brain
                .dispatch(BrainMsg::Watch(format!(
                    "{{\"kind\":9,\"created_at\":{ts}}}"
                )))
                .await;
        }
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert_eq!(
            journal.matches("\"stage\":\"dedup\"").count(),
            2,
            "first 90 and 95 trace; the repeated 95 stays silent: {journal}"
        );
    }

    /// bd-5qyt: a failing append must not consume a seq and must not be silent.
    ///
    /// Live shape this reproduces: `doorbell-journal-flywheel-opencode.jsonl`
    /// ran contiguous through 4,633 sequenced rows except one hole — seq 153410
    /// then 153427, with the 16 values between them in NO journal file and no
    /// retention record covering the range. `journal()` took the seq, advanced
    /// `next_seq` and threw the append's error away (`let _ = ...`), so each
    /// failed write burned a seq and the hole was its only trace.
    ///
    /// The fault is real, not mocked: a DIRECTORY sits where the active journal
    /// belongs, so `private_append` fails with EISDIR while the audit sidecar
    /// (a different subdirectory) stays writable.
    #[tokio::test]
    async fn journal_write_failure_is_named_not_swallowed() {
        let (mut brain, _rx, _tx) = test_brain("writefail", vec![]);
        let blocked = brain.store.journal_file("w");
        std::fs::create_dir_all(&blocked).expect("block the journal path");
        let start = brain.next_seq;
        let sweep = serde_json::json!({"rt_kind": "Sweep"});
        for _ in 0..3 {
            brain.journal(&sweep, &[], serde_json::json!({}), &[]);
        }
        assert_eq!(
            brain.next_seq, start,
            "a seq that was never written must not be consumed; the failed rows owe {start}"
        );

        let audit = std::fs::read_to_string(brain.store.audit_file("w")).expect("audit sidecar");
        let rows: Vec<serde_json::Value> = audit
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|v: &serde_json::Value| {
                v.get("kind").and_then(|k| k.as_str()) == Some("journal_write_failed")
            })
            .collect();
        let last = rows.last().expect("the loss must NAME itself, not vanish");
        assert_eq!(
            last.get("seq").and_then(|s| s.as_u64()),
            Some(start),
            "the row carries the seq that could not be written: {last}"
        );
        assert_eq!(
            last.get("failed").and_then(|f| f.as_u64()),
            Some(3),
            "and the counter, so a run of failures reads as N not as one: {last}"
        );
        assert!(
            last.get("error").is_some_and(|e| !e.is_null()),
            "nor may it drop WHY: {last}"
        );

        std::fs::remove_dir_all(&blocked).expect("unblock the journal path");
        brain.journal(&sweep, &[], serde_json::json!({}), &[]);
        assert_eq!(
            brain.next_seq,
            start + 1,
            "the row landed, so now the seq is spent"
        );
        assert_eq!(
            brain.store.last_seq("w"),
            start,
            "the recovered row carries exactly the seq that failed"
        );
        assert!(
            brain.store.fold_workspace("w").torn.is_none(),
            "a recovered write leaves no tear behind"
        );
        assert_eq!(
            brain.journal_write_failed, 3,
            "and the tally survives the recovery"
        );
    }

    /// bd-jcys: the D4.2 write-ahead flush is the one durable write whose
    /// failure changes what the journal can PROVE — the decided row is in the
    /// file but not on disk, so a crash there leaves a delivered prompt with
    /// nothing to explain it. Two things must both hold: the failure is NAMED,
    /// and the prompt still goes out. Withholding it would lose the wake
    /// outright, because `on_watch` has already advanced the plane-S cursor
    /// and the next ring would dedup it away.
    ///
    /// The fault is real, not mocked: a FILE sits where the `journals/`
    /// directory belongs, so every journal open fails ENOTDIR while the audit
    /// sidecar (a different subdirectory) stays writable.
    #[tokio::test]
    async fn a_failed_write_ahead_flush_is_named_and_delivery_survives() {
        let prompt = Effect::Prompt {
            event_id: None,
            pin: "p".into(),
            line: "[doorbell] write-ahead".into(),
        };
        let (mut brain, _rx, _tx) = test_brain("sync-fail", vec![vec![prompt]]);
        let (sink, _sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));
        let journals = brain
            .store
            .journal_file("w")
            .parent()
            .expect("journals dir")
            .to_path_buf();
        std::fs::create_dir_all(journals.parent().expect("state dir")).expect("state dir");
        std::fs::write(&journals, b"not a directory").expect("block the journals dir");

        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;

        let audit = std::fs::read_to_string(brain.store.audit_file("w")).expect("audit sidecar");
        assert!(
            audit.contains("\"kind\":\"journal_sync_failed\""),
            "the failed write-ahead flush must NAME itself, not vanish: {audit}"
        );
        assert_eq!(
            brain.stats.injects, 1,
            "delivery is NOT withheld: the cursor already advanced, so dropping the \
             prompt here would lose the wake outright"
        );
    }

    /// bd-jcys (Tier 2): the plane-S cursor save is the durable write in
    /// `on_watch` that could fail with no trace — `persist_wake` already warned
    /// on its own path, the direct `save_wake` sites did not. A lost cursor
    /// re-rings old history (the safe direction), so this NAMES the failure
    /// instead of changing the flow; the audit sidecar is what an operator
    /// greps.
    #[tokio::test]
    async fn a_failed_wake_save_is_named_in_the_audit() {
        let (mut brain, _rx, _tx) = test_brain("wake-save-fail", vec![vec![]]);
        std::fs::create_dir_all(brain.store.wake_file("w")).expect("block the wake state");

        brain
            .dispatch(BrainMsg::Watch("{\"kind\":9,\"created_at\":100}".into()))
            .await;

        assert!(
            brain.wake.plane_s_cursor_us.is_some(),
            "premise: the cursor advanced in memory, so a save really was attempted"
        );
        let audit = std::fs::read_to_string(brain.store.audit_file("w")).expect("audit sidecar");
        assert!(
            audit.contains("\"kind\":\"wake_save_failed\""),
            "a wake-state write that did not land must NAME itself: {audit}"
        );
    }

    /// Two events can share a second, so `created_at` alone cannot answer
    /// "already handled": the plane-S high-water is compared with `<=`, so the
    /// second event of a busy second was dropped before it could ring, and the
    /// drop was indistinguishable from a redelivery. The id is what
    /// distinguishes them, and a redelivery must still dedup.
    #[tokio::test]
    async fn two_events_in_one_second_both_ring_and_a_redelivery_still_dedups() {
        let line = |id: &str| format!("{{\"kind\":9,\"id\":\"{id}\",\"created_at\":100}}");
        let (mut brain, _rx, _tx) = test_brain(
            "same-second",
            vec![
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "first".into(),
                }],
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "second".into(),
                }],
            ],
        );
        let (sink, sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));

        brain.dispatch(BrainMsg::Watch(line("aa"))).await;
        brain.dispatch(BrainMsg::Watch(line("bb"))).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if sent.lock().expect("sent log").len() >= 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both same-second wakes deliver");
        let lines: Vec<String> = sent
            .lock()
            .expect("sent log")
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        assert_eq!(
            lines,
            vec!["first".to_string(), "second".to_string()],
            "a shared second is not a reason to drop a wake: {lines:?}"
        );

        brain.dispatch(BrainMsg::Watch(line("aa"))).await;
        assert_eq!(
            sent.lock().expect("sent log").len(),
            2,
            "a redelivered event is still suppressed"
        );
        assert_eq!(
            brain
                .stats
                .rings_by_class
                .get("watch_dedup")
                .copied()
                .unwrap_or_default(),
            1,
            "the suppression stays counted, never silent"
        );
    }

    #[tokio::test]
    async fn same_msg_log_same_effects_determinism() {
        use crate::host::CoreReducer;
        let log = [
            BrainMsg::Watch("{\"kind\":9,\"tags\":[[\"p\",\"x\"]]}".into()),
            BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "idle".into(),
            },
            BrainMsg::AckSlaTick { now: 2_000_000 },
        ];
        let mut runs = Vec::new();
        for _ in 0..2 {
            let mut reducer = CoreReducer::default();
            let seq: Vec<String> = log
                .iter()
                .map(|m| format!("{:?}", reducer.tick_effects(m)))
                .collect();
            runs.push(seq.join("\n"));
        }
        assert_eq!(runs[0], runs[1]);
    }

    #[tokio::test]
    async fn degraded_is_journaled_and_counted() {
        let (mut brain, _rx, _tx) = test_brain("degraded", vec![]);
        brain
            .dispatch(BrainMsg::Degraded {
                source: "bus-watch".into(),
                reason: "producer closed".into(),
            })
            .await;
        assert_eq!(brain.stats.degraded, 1);
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert!(journal.contains("bus-watch"), "{journal}");
    }
    #[tokio::test]
    async fn hear_stamps_on_delivered_not_enqueue() {
        let (mut brain, _rx, _tx) = test_brain("hear", vec![]);
        let (sink, _sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));
        let queued = brain
            .deliver_prompts(vec![("p".into(), "[doorbell] x".into())])
            .await;
        assert_eq!(queued, vec!["p".to_string()]);
        assert!(
            brain
                .hear
                .expire(1_000_000 + 200, doorbell_core::UNANSWERED_SECS)
                .is_empty(),
            "enqueue must not stamp hear"
        );
        brain
            .dispatch(BrainMsg::Delivered { pin: "p".into() })
            .await;
        let expired = brain
            .hear
            .expire(1_000_000 + 121, doorbell_core::UNANSWERED_SECS);
        assert!(expired.contains(&"p".to_string()), "Delivered stamps hear");
    }

    #[tokio::test]
    async fn budget_cuts_fourth_prompt_in_window() {
        let prompts = (0..4)
            .map(|i| Effect::Prompt {
                event_id: None,
                pin: "p".into(),
                line: format!("m{i}"),
            })
            .collect::<Vec<_>>();
        let (mut brain, _rx, tx) = test_brain("budget", vec![prompts]);
        let (sink, sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));
        brain
            .dispatch(BrainMsg::Fact {
                summary: "x".into(),
            })
            .await;
        drop(tx);
        assert_eq!(brain.stats.injects, 3);
        assert_eq!(brain.stats.drops, 1);
        let sent_n = async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let n = sent.lock().unwrap_or_else(|e| e.into_inner()).len();
                if n >= 3 || tokio::time::Instant::now() >= deadline {
                    return n;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        .await;
        assert_eq!(sent_n, 3);
    }

    /// S4 parity gate at the rt level: the same Msg log through two Brains
    /// yields byte-identical journals modulo `ts` (the normalize rule the
    /// Phase-1 harness pins: strip `ts` fields only).
    #[tokio::test]
    async fn parity_same_log_same_journal_modulo_ts() {
        async fn run_once(tag: &str) -> String {
            let prompt = Effect::Prompt {
                event_id: None,
                pin: "p".into(),
                line: "[doorbell] ring".into(),
            };
            let (mut brain, _rx, _tx) = test_brain(
                tag,
                vec![
                    vec![prompt.clone()],
                    vec![],
                    vec![prompt.clone()],
                    vec![],
                    vec![],
                ],
            );
            let (sink, _sent) = MockSink::new();
            brain.sinks.add_sink("hc", Box::new(sink));
            for msg in [
                BrainMsg::Watch("{\"kind\":9}".into()),
                BrainMsg::Sweep { now: 1_000_100 },
                BrainMsg::PlaneStatus {
                    pin: "p".into(),
                    status: "idle".into(),
                },
                BrainMsg::AckSlaTick { now: 1_000_200 },
                BrainMsg::Fact {
                    summary: "s".into(),
                },
            ] {
                brain.dispatch(msg).await;
            }
            let raw = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
            raw.lines()
                .map(|l| {
                    let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
                    v.as_object_mut().unwrap().remove("ts");
                    serde_json::to_string(&v).unwrap()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        let a = run_once("parity-a").await;
        let b = run_once("parity-b").await;
        assert_eq!(a, b, "same Msg log must journal identically modulo ts");
        assert!(a.contains("\"stage\":\"decided\""), "{a}");
    }

    /// E12 crash harness (process-crash model): task abort (no graceful
    /// shutdown — the async equivalent of SIGKILL for journal semantics:
    /// bd-watch-restart-metric-8dvf: `RestartWatch` must mean a restart
    /// HAPPENED. `restart_wake` is single-flight, so a burst of deaths
    /// collapses to ONE restart; journaling every death as a restart is what
    /// made `/health` count ~40x the truth (1,412 rows inside one 300s window
    /// against 35 real watch cycles) and made `restarted: true` a lie for
    /// every coalesced death.
    #[tokio::test]
    async fn a_burst_of_deaths_leaves_one_restart_row() {
        let (mut brain, _rx, _tx) =
            test_brain("restart-burst", vec![vec![Effect::RestartWatch]; 50]);
        for _ in 0..50 {
            brain.dispatch(BrainMsg::Died).await;
        }
        assert_eq!(
            brain.host.restarts, 1,
            "50 deaths, one restart: the single-flight gate must coalesce the burst"
        );
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        let rows = journal.matches("\"RestartWatch\"").count();
        assert_eq!(
            rows, 1,
            "the journal carries one RestartWatch row per ACTUAL restart; counting deaths \
             instead is the defect. got {rows} rows:\n{journal}"
        );
        let inert_rows = journal.matches("\"restarted\":false").count();
        assert!(
            inert_rows <= 8,
            "49 coalesced deaths wrote {inert_rows} rows; they are counted, not logged: {journal}"
        );
        assert!(
            journal.contains("\"coalesced_deaths\":32"),
            "a coalesced death must say it did not restart AND how many deaths it stands \
             for, or the count is lost: {journal}"
        );
    }

    /// The other half of the same defect: the metric was fixed, the AMPLIFIER
    /// was not. A `Died` that decided nothing still wrote a full row, so a
    /// storm of coalesced deaths cost one row each. Measured live on
    /// 2026-09-28 (04:32Z): one minute wrote 2,120 rows for 8 real restarts,
    /// 2,112 of them `restarted: false` with `actions: []` — 265 rows of noise
    /// per restart of truth. A journal that grows with the storm cannot be used
    /// to measure the storm.
    ///
    /// An inert death is a COUNT, not a row: it is journaled at 1, 2, 4, 8 ...
    /// so the cost is log2(deaths), and the row states how many deaths it
    /// stands for, so nothing is silent. The trade is a bounded count
    /// (a row stands for at most 2x the deaths since the previous row), which
    /// is the price of keeping the journal readable during the incident it
    /// is supposed to describe.
    #[tokio::test]
    async fn inert_deaths_are_counted_not_journaled_one_row_each() {
        const DEATHS: usize = 4096;
        let (mut brain, _rx, _tx) = test_brain("inert-died", vec![vec![]; DEATHS]);
        for _ in 0..DEATHS {
            brain.dispatch(BrainMsg::Died).await;
        }
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        let rows = journal.matches("\"rt_kind\":\"Died\"").count();
        assert!(
            rows <= 16,
            "{DEATHS} inert deaths wrote {rows} rows; the geometric count caps at \
             log2({DEATHS}) = 12. One row per death is the amplifier that made a \
             2,120-row minute out of 8 restarts"
        );
        let covered: usize = journal
            .lines()
            .filter_map(|l| l.split("\"coalesced_deaths\":").nth(1))
            .filter_map(|rest| {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<usize>().ok()
            })
            .sum();
        assert!(
            covered >= DEATHS / 2,
            "the rows must account for the deaths they stand for: {covered} of {DEATHS}
             (a geometric cap can undershoot by at most one doubling)"
        );
    }

    /// flushed bytes survive, in-flight vanishes) then rebuild on the same
    /// store. Acceptance: 3 prompts fill the window, restart, 0 further
    /// prompts in the window; refold reproduces budget state exactly; every
    /// delivered intent was journaled first; resume logs its redelivery row.
    #[tokio::test]
    async fn crash_abort_restart_grants_zero_further_in_window() {
        let dir = std::env::temp_dir().join(format!("dbrt-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mk = || {
            let (tx, rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
            let mut host = MockHost::new(1_000_000);
            host.script = VecDeque::from(vec![
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "m1".into(),
                }],
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "m2".into(),
                }],
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "m3".into(),
                }],
                vec![Effect::Prompt {
                    event_id: None,
                    pin: "p".into(),
                    line: "m4".into(),
                }],
            ]);
            let sinks = SinkManager::new(tx.clone());
            let brain = Brain::new(BrainConfig::new("w"), host, FileStore::new(&dir), sinks);
            (brain, rx, tx)
        };
        let (mut a, _rx, _tx) = mk();
        let (sink, _sent) = MockSink::new();
        a.sinks.add_sink("hc", Box::new(sink));
        a.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        a.dispatch(BrainMsg::Fact {
            summary: "x".into(),
        })
        .await;
        a.dispatch(BrainMsg::Fact {
            summary: "y".into(),
        })
        .await;
        assert_eq!(a.stats.injects, 3);
        drop(a);
        let (mut b, _rx2, _tx2) = mk();
        let budget = b.budgets.get("p").expect("refolded budget row");
        assert_eq!(budget.prompts.len(), 3, "all three grants refolded");
        let (sink_b, _sent_b) = MockSink::new();
        b.sinks.add_sink("hc", Box::new(sink_b));
        let journal = std::fs::read_to_string(b.store.journal_file("w")).unwrap();
        assert!(
            journal.contains("\"stage\":\"resume\"") && journal.contains("\"redelivered\":3"),
            "resume logs its redelivery set: {journal}"
        );
        let before = b.store.last_seq("w");
        b.dispatch(BrainMsg::Fact {
            summary: "z".into(),
        })
        .await;
        assert!(b.store.last_seq("w") > before, "seq advances past resume");
        assert_eq!(b.stats.injects, 0, "window stays shut after restart");
        assert_eq!(b.stats.drops, 1, "fourth prompt drops loudly");
        for line in ["m1", "m2", "m3"] {
            assert!(journal.contains(line), "delivered intent journaled: {line}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clock-jump: an NTP step mid-run never reorders the journal; seq
    /// stays monotonic and the sweep cadence re-stabilizes.
    #[tokio::test]
    async fn clock_jump_keeps_order_and_restabilizes() {
        let decided = vec![Effect::Audit {
            kind: "k".into(),
            pin: "p".into(),
            detail: serde_json::json!({}),
        }];
        let (mut brain, _rx, _tx) = test_brain("clock", vec![vec![], decided, vec![]]);
        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        brain.host.now = 1_000_000 + 86_400;
        brain
            .dispatch(BrainMsg::Sweep {
                now: 1_000_000 + 86_400,
            })
            .await;
        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        let raw = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        let mut seqs = Vec::new();
        for line in raw.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(s) = v.get("seq").and_then(|s| s.as_u64()) {
                seqs.push(s);
            }
        }
        assert!(seqs.len() >= 3, "rows journaled across the jump");
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "seq strictly consecutive across the clock jump: {seqs:?}"
        );
        assert!(brain.sweep_last.is_some(), "sweep re-stabilizes after jump");
    }

    /// Clock-jump, the hard direction (bd-d60u, ADR-0012 §8 Phase 4): an NTP
    /// step BACKWARDS mid-run must not buy extra prompts, rewind liveness, or
    /// reorder the journal, and the deadline state must re-stabilize once the
    /// clock settles forward again. The forward-only case above misses the
    /// direction that can actually mis-grant a budget.
    #[tokio::test]
    async fn clock_step_backwards_buys_nothing_and_re_stabilizes() {
        let t = 1_700_000_000u64;
        let one = |line: &str| {
            vec![Effect::Prompt {
                event_id: None,
                pin: "p".into(),
                line: line.into(),
            }]
        };
        let burn_window: Vec<Effect> = (0..4)
            .map(|i| Effect::Prompt {
                event_id: None,
                pin: "p".into(),
                line: format!("m{i}"),
            })
            .collect();
        let (mut brain, _rx, _tx) = test_brain(
            "clock-step",
            vec![burn_window, vec![], vec![], vec![], one("forward")],
        );
        let (sink, _sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));
        brain.host.now = t;
        brain
            .dispatch(BrainMsg::Fact {
                summary: "x".into(),
            })
            .await;
        assert_eq!((brain.stats.injects, brain.stats.drops), (3, 1));
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "idle".into(),
            })
            .await;
        assert_eq!(brain.wake.last_wake_unix, Some(t), "liveness stamped at T");

        brain.host.now = t - 3_600;
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "done".into(),
            })
            .await;
        assert_eq!(
            brain.stats.injects, 3,
            "a backwards step must never refill the budget window"
        );
        assert_eq!(
            brain.wake.last_wake_unix,
            Some(t),
            "the liveness ratchet refuses a behind stamp (bd-7qg6)"
        );

        brain.host.now = t + 7_200;
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "idle".into(),
            })
            .await;
        assert_eq!(brain.wake.last_wake_unix, Some(t + 7_200));
        brain
            .dispatch(BrainMsg::Fact {
                summary: "x".into(),
            })
            .await;
        assert_eq!(brain.stats.injects, 4, "budget window rolled forward");

        let raw = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        let seqs: Vec<u64> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v.get("seq").and_then(|s| s.as_u64()))
            .collect();
        assert!(seqs.len() >= 4, "every step journaled: {seqs:?}");
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "seq stays consecutive and increasing across both steps: {seqs:?}"
        );
    }

    #[tokio::test]
    async fn cursorless_pin_sweeps_from_redelivery_floor() {
        let now = 1_700_000_000u64;
        let floor = doorbell_core::redelivery_floor_rfc3339(now);
        let row_us = now as i64 * 1_000_000;
        let (mut brain, _rx, _tx) = test_brain("floor", vec![vec![Effect::FetchInbox], vec![]]);
        brain.host.now = now;
        brain.host.pin_list = vec!["p".into()];
        brain.host.inbox.insert(
            ("p".into(), floor.clone()),
            // A genuinely non-wake row: an ack-only delivery. Routine mail is
            // a wake since bd-0uzrq.8 (dropping it lost a captain brief), so
            // only ack-only rows are free to advance the cursor.
            vec![serde_json::json!({
                "ack_only": true,
                "importance": "normal",
                "created_ts_us": row_us,
                "body_md": "hi",
            })],
        );
        assert!(!brain.wake.since_by_pin.contains_key("p"));
        brain.dispatch(BrainMsg::Sweep { now }).await;
        let hw = brain
            .wake
            .since_by_pin
            .get("p")
            .expect("cursor initializes from the floor backfill");
        assert_eq!(
            doorbell_core::parse_rfc3339_micros(hw).expect("cursor parses"),
            row_us,
            "high-water follows the row's rumor time, not wall-clock now"
        );
    }

    /// wave17-b: a wake row that produced NO decision must NOT advance the
    /// cursor. The queued line lives only in the fold's model; a host restart
    /// used to lose it after the cursor had moved past it (live: every
    /// done-mail eaten, `rings=0`).
    #[tokio::test]
    async fn undecided_wake_row_does_not_advance_the_cursor() {
        let now = 1_700_000_000u64;
        let floor = doorbell_core::redelivery_floor_rfc3339(now);
        let row_us = now as i64 * 1_000_000;
        let (mut brain, _rx, _tx) = test_brain("keep", vec![vec![Effect::FetchInbox], vec![]]);
        brain.host.now = now;
        brain.host.pin_list = vec!["p".into()];
        brain.host.inbox.insert(
            ("p".into(), floor.clone()),
            vec![serde_json::json!({
                "importance": "urgent",
                "created_ts_us": row_us,
                "body_md": "hi",
            })],
        );
        brain.dispatch(BrainMsg::Sweep { now }).await;
        assert!(
            !brain.wake.since_by_pin.contains_key("p"),
            "an undecided wake must stay behind the cursor (no silent eat)"
        );
    }

    /// Standing determinism fuzz (no new deps: xorshift64): the same seeded
    /// random Msg log through two reducers yields identical effect traces.
    /// Runs in unit CI from Phase 0 onward per section 8.
    #[test]
    fn determinism_fuzz_seeded_log_same_effects() {
        use crate::host::CoreReducer;
        fn next_rand(state: &mut u64) -> u64 {
            let mut x = *state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *state = x;
            x
        }
        fn gen_msg(rng: &mut u64) -> BrainMsg {
            let kinds = [9u16, 14, 20001, 30013, 43001, 30031, 39002, 7];
            match next_rand(rng) % 9 {
                0 => BrainMsg::Watch(format!(
                    "{{\"kind\":{},\"tags\":[[\"p\",\"t\"]]}}",
                    kinds[(next_rand(rng) % 8) as usize]
                )),
                1 => BrainMsg::PlaneStatus {
                    pin: ["p", "q"][(next_rand(rng) % 2) as usize].into(),
                    status: ["idle", "working", "done", "blocked", "mystery"]
                        [(next_rand(rng) % 5) as usize]
                        .into(),
                },
                2 => BrainMsg::Died,
                3 => BrainMsg::Sweep {
                    now: 1_000_000 + next_rand(rng) % 100_000,
                },
                4 => BrainMsg::RoleBound {
                    role: "coder".into(),
                    pin: "p".into(),
                },
                5 => BrainMsg::Fact {
                    summary: format!("f{}", next_rand(rng) % 10),
                },
                6 => BrainMsg::AckSlaTick {
                    now: 1_000_000 + next_rand(rng) % 100_000,
                },
                7 => BrainMsg::Presence {
                    pin: "p".into(),
                    state: "idle".into(),
                },
                _ => BrainMsg::SlaFired { posts: vec![] },
            }
        }
        for seed in [1u64, 42, 0xDEAD_BEEF] {
            let mut traces = Vec::new();
            for _ in 0..2 {
                let mut rng = seed;
                let mut reducer = CoreReducer::default();
                let mut trace = Vec::new();
                for _ in 0..300 {
                    let m = gen_msg(&mut rng);
                    trace.push(format!("{:?}", reducer.tick_effects(&m)));
                }
                traces.push(trace.join("\n"));
            }
            assert_eq!(traces[0], traces[1], "seed {seed} must replay identically");
        }
    }

    /// S3 deaf-service digest: an unanswered pin batches a line naming
    /// subscriber + attempts (corr binds in bd-asr2.11 with receipts).
    #[tokio::test]
    async fn deaf_digest_names_subscriber_and_attempts() {
        let (mut brain, _rx, _tx) = test_brain("deaf", vec![vec![]]);
        brain.cfg.digest_threshold = 1;
        let (sink, sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));
        brain.hear.note_inject("svc", None, 1_000_000);
        brain
            .dispatch(BrainMsg::Sweep {
                now: 1_000_000 + 120,
            })
            .await;
        assert_eq!(brain.hear.unanswered_pins(), vec!["svc".to_string()]);
        brain.digest_push("svc".into(), "queued".into());
        brain.maybe_flush_digest().await;
        let body = async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let text = {
                    let s = sent.lock().unwrap_or_else(|e| e.into_inner());
                    s.iter()
                        .map(|(_, l)| l.clone())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                if text.contains("[deaf]") || tokio::time::Instant::now() >= deadline {
                    return text;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        .await;
        assert!(
            body.contains("[deaf] subscriber=workspace:w attempts=1"),
            "deaf digest names subscriber+attempts: {body}"
        );
    }

    /// bd-bk9t: a pane-less subscribed pin's wake has no prompt route, so the
    /// `Watch` arm must fold it into the digest batch. Regression: that arm
    /// collected prompts only, so a `Digest` effect was silently dropped —
    /// the exact failure mode this bead exists to remove.
    #[tokio::test]
    async fn digest_bound_wake_from_watch_reaches_the_batch() {
        let line = "[doorbell] plane-P urgent. Pull: toron mail inbox --since t";
        let (mut brain, _rx, _tx) = test_brain(
            "bk9t-watch",
            vec![vec![Effect::Digest {
                pin: "OperatorPin".into(),
                line: line.into(),
                why: "no_route".into(),
            }]],
        );
        brain
            .dispatch(BrainMsg::Watch("{\"kind\":9,\"created_at\":2}".into()))
            .await;
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert!(
            journal.contains("\"why\":\"no_route\""),
            "the watch row must carry the digest reason: {journal}"
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        brain
            .dispatch(BrainMsg::Ask {
                kind: AskKind::Digest,
                reply: tx,
            })
            .await;
        match rx.await.expect("digest reply") {
            AskReply::Digest(batch) => assert!(
                batch.iter().any(|(p, l)| p == "OperatorPin" && l == line),
                "the pane-less pin's wake must be in the digest: {batch:?}"
            ),
            other => panic!("expected Digest reply, got {other:?}"),
        }
        assert_eq!(brain.stats.injects, 0, "no prompt route exists");
    }

    /// bd-bk9t: a digest batch for a pin with no route must NOT be shipped
    /// to a sink. The hc sink misses an unknown pin and returns `Ok`, so
    /// shipping would stamp a `Delivered` receipt (and a hear stamp) for a
    /// wake nobody received. The journal row is the durable record.
    #[tokio::test]
    async fn digest_batch_for_a_pane_less_pin_is_not_shipped_to_a_sink() {
        let (mut brain, _rx, _tx) = test_brain(
            "bk9t-skip",
            vec![vec![Effect::Digest {
                pin: "OperatorPin".into(),
                line: "[doorbell] plane-P urgent".into(),
                why: "no_route".into(),
            }]],
        );
        brain.cfg.digest_threshold = 1;
        let (sink, sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));
        brain.host.unroutable.insert("OperatorPin".into());
        assert!(!brain.host.has_route("OperatorPin"));
        brain
            .dispatch(BrainMsg::Watch("{\"kind\":9,\"created_at\":2}".into()))
            .await;
        assert!(
            sent.lock().expect("sent log").is_empty(),
            "a batch with no destination must not be shipped: {:?}",
            sent.lock().expect("sent log")
        );
        assert_eq!(
            brain.stats.injects, 0,
            "nothing was delivered, so no inject intent may be counted"
        );
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert!(
            journal.contains("\"why\":\"no_route\""),
            "the wake stays journal-recorded: {journal}"
        );
    }

    /// Second consumer (bd-asr2.11): a subscribed human identity receives
    /// a wake with zero swarm alive — no panes, no processes, no dispatcher
    /// beyond this Brain. The journal row carries the subscriber tag so
    /// `why --subscriber` replays it.
    #[tokio::test]
    async fn human_subscription_wake_needs_no_swarm() {
        use crate::subscribe::*;
        let dir = std::env::temp_dir().join(format!("dbrt-human-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SubscriptionStore::open(&dir);
        let sub = Subscription {
            id: "human-1".into(),
            identity: Identity::Npub("npub1human".into()),
            scope: Scope::One("w".into()),
            filter: Filter::default(),
            sink: SinkKind::Hc,
            budget: Budget::default_530(),
            project: Some("proj".into()),
            endpoint: None,
        };
        let sub = store
            .subscribe(Actor::operator(), sub, "w")
            .expect("operator subscribes human");
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let mut host = MockHost::new(1_000_000);
        host.script = VecDeque::from(vec![vec![Effect::Prompt {
            event_id: None,
            pin: "human".into(),
            line: "[doorbell] hold expired".into(),
        }]]);
        let sinks = SinkManager::new(tx.clone());
        let mut cfg = BrainConfig::new("w");
        cfg.subscriber = sub.identity.tag();
        cfg.budget = sub.budget;
        let mut brain = Brain::new(cfg, host, FileStore::new(&dir), sinks);
        let (sink, sent) = MockSink::new();
        brain.sinks.add_sink("hc", Box::new(sink));
        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        assert_eq!(brain.stats.injects, 1, "human wake delivers");
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert!(
            journal.contains("\"subscriber\":\"npub:npub1human\""),
            "wake row tagged: {journal}"
        );
        drop(tx);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = sent;
    }

    #[tokio::test]
    async fn status_transition_gate_sees_pre_tick_status() {
        let (mut brain, _rx, _tx) = test_brain("statusgate", vec![]);
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "working".into(),
            })
            .await;
        let first = brain.wake.last_wake_unix;
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "idle".into(),
            })
            .await;
        let second = brain.wake.last_wake_unix;
        assert!(
            second.is_some(),
            "working -> idle transition must ratchet last_wake (prev status = working)"
        );
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "idle".into(),
            })
            .await;
        assert_eq!(
            brain.wake.last_wake_unix, second,
            "already-idle repeats must not advance liveness"
        );
        assert!(first.is_none() || first == second);
        assert!(second.is_some());
    }

    #[tokio::test]
    async fn status_wake_row_names_the_status_for_replay() {
        let (mut brain, _rx, _tx) = test_brain("statuswhy", vec![]);
        brain
            .dispatch(BrainMsg::PlaneStatus {
                pin: "p".into(),
                status: "idle".into(),
            })
            .await;
        let raw = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        let row = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|v| v["msg"]["rt_kind"] == "PlaneStatus")
            .expect("status transition must be journaled");
        assert_eq!(row["msg"]["pin"], "p");
        assert_eq!(
            row["msg"]["status"], "idle",
            "why must replay the status, not just the pin"
        );
        assert!(
            brain.wake.last_wake_unix.is_some(),
            "the same observed transition stamps last_wake"
        );
    }

    #[tokio::test]
    async fn digest_ask_resets_interval_clock() {
        let (mut brain, _rx, _tx) = test_brain("digestask", vec![]);
        brain.digest_push("p".into(), "[doorbell] one".into());
        assert!(brain.digest_since.is_some());
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        brain
            .dispatch(BrainMsg::Ask {
                kind: AskKind::Digest,
                reply: rtx,
            })
            .await;
        let AskReply::Digest(lines) = rrx.await.unwrap() else {
            panic!("digest reply");
        };
        assert_eq!(lines.len(), 1);
        assert!(
            brain.digest_since.is_none(),
            "taking the batch must reset the interval clock"
        );
    }

    #[tokio::test]
    async fn sink_degrade_restarts_and_requeues_with_factory() {
        let (deg_tx, mut deg_rx) = tokio::sync::mpsc::channel::<BrainMsg>(16);
        let (mut brain, _rx, _tx2) = test_brain("sinkrestart", vec![]);
        brain.sinks = SinkManager::new(deg_tx);
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct FailingThenOk {
            n: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            fail: bool,
        }
        impl crate::sinks::PromptSink for FailingThenOk {
            fn send(&mut self, _pin: &str, _line: &str) -> Result<(), crate::error::RtError> {
                if self.fail {
                    return Err(crate::error::RtError::EndpointDown);
                }
                self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn name(&self) -> &str {
                "failing-then-ok"
            }
        }
        let n2 = n.clone();
        brain.sinks.add_sink_with_factory(
            "hc",
            Box::new(FailingThenOk {
                n: n.clone(),
                fail: true,
            }),
            move || {
                Box::new(FailingThenOk {
                    n: n2.clone(),
                    fail: false,
                })
            },
        );
        brain
            .deliver_prompts(vec![("p".into(), "[doorbell] wake".into())])
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(brain.sinks.parked("hc").await, "failed adapter parks");
        assert!(
            matches!(
                brain.sinks.send("hc", "p", "[doorbell] two").await,
                SendOutcome::Shed { .. }
            ),
            "parked sink sheds"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while let Ok(m) = deg_rx.try_recv() {
            brain.dispatch(m).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            n.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "restarted adapter delivered the requeued intent, got {}",
            n.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(!brain.sinks.parked("hc").await, "sink revived");
        assert!(
            matches!(
                brain.sinks.send("hc", "p", "[doorbell] three").await,
                SendOutcome::Queued
            ),
            "revived sink queues"
        );
    }

    #[tokio::test]
    async fn sink_degrade_without_factory_sheds_to_digest() {
        let (deg_tx, _deg_rx) = tokio::sync::mpsc::channel::<BrainMsg>(16);
        let (mut brain, _rx, _tx2) = test_brain("sinknofactory", vec![]);
        brain.sinks = SinkManager::new(deg_tx);
        let (mut sink, _sent) = MockSink::new();
        sink.fail_after = Some(0);
        brain.sinks.add_sink("hc", Box::new(sink));
        brain
            .deliver_prompts(vec![("p".into(), "[doorbell] wake".into())])
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(brain.sinks.parked("hc").await);
        brain
            .dispatch(BrainMsg::Degraded {
                source: "sink:hc".into(),
                reason: "delivery failed: endpoint down".into(),
            })
            .await;
        assert!(brain.sinks.parked("hc").await, "no factory: stays parked");
    }
}

#[derive(Debug, Clone)]
struct PendingRing {
    npub: String,
    injected_unix: u64,
}

/// Heard-or-deaf: a ring that is not heard is a conflict-shaped event.
#[derive(Debug, Default, Clone)]
pub struct HearTracker {
    pending: BTreeMap<String, PendingRing>,
    unanswered: BTreeSet<String>,
    /// pin → npub, kept after expire so a late post still clears deaf.
    npubs: BTreeMap<String, String>,
}

impl HearTracker {
    pub fn note_inject(&mut self, pin: &str, npub: Option<&str>, now: u64) {
        self.unanswered.remove(pin);
        let npub = npub.unwrap_or("").to_lowercase();
        if !npub.is_empty() {
            self.npubs.insert(pin.to_string(), npub.clone());
        }
        self.pending.insert(
            pin.to_string(),
            PendingRing {
                npub,
                injected_unix: now,
            },
        );
    }

    /// Fill npubs learned later (whois on the 1s tick).
    pub fn remember_npub(&mut self, pin: &str, npub: Option<&str>) {
        let Some(n) = npub.filter(|s| !s.is_empty()) else {
            return;
        };
        let n = n.to_lowercase();
        self.npubs.insert(pin.to_string(), n.clone());
        if let Some(ring) = self.pending.get_mut(pin) {
            ring.npub = n;
        }
    }

    pub fn note_heard_pin(&mut self, pin: &str) {
        self.pending.remove(pin);
        self.unanswered.remove(pin);
    }

    pub fn note_heard_pubkey(&mut self, pubkey: &str) {
        let key = pubkey.to_lowercase();
        let mut pins: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, r)| !r.npub.is_empty() && r.npub == key)
            .map(|(p, _)| p.clone())
            .collect();
        pins.extend(
            self.npubs
                .iter()
                .filter(|(_, n)| *n == &key)
                .map(|(p, _)| p.clone()),
        );
        pins.sort();
        pins.dedup();
        for pin in pins {
            self.note_heard_pin(&pin);
        }
    }

    /// Move timed-out pending pins to unanswered. Returns newly expired pins.
    pub fn expire(&mut self, now: u64, timeout_secs: u64) -> Vec<String> {
        let mut expired = Vec::new();
        self.pending.retain(|pin, ring| {
            if now.saturating_sub(ring.injected_unix) >= timeout_secs {
                expired.push(pin.clone());
                false
            } else {
                true
            }
        });
        for pin in &expired {
            self.unanswered.insert(pin.clone());
        }
        expired
    }

    pub fn unanswered_pins(&self) -> Vec<String> {
        self.unanswered.iter().cloned().collect()
    }

    /// Resolve every ring for a pin that has no live pane: "no pane, nothing
    /// to ring". A gone or retired pin can never answer, so its ring must not
    /// sit in `unanswered` forever and page the captain every digest
    /// (bd-3vkpe). Returns the pins it resolved (sorted).
    pub fn retain_live(&mut self, live: &[String]) -> Vec<String> {
        let live: BTreeSet<&String> = live.iter().collect();
        let gone: Vec<String> = self
            .pending
            .keys()
            .chain(self.unanswered.iter())
            .filter(|pin| !live.contains(pin))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for pin in &gone {
            self.pending.remove(pin);
            self.unanswered.remove(pin);
        }
        gone
    }

    pub fn seed_unanswered(&mut self, pins: impl IntoIterator<Item = String>) {
        self.unanswered.extend(pins);
    }
}

#[cfg(test)]
mod hear_tests {
    use super::*;

    /// bd-3vkpe: a pin with no pane can never answer, so its ring is resolved
    /// as "no pane, nothing to ring" — never an unanswered conflict that pages
    /// the captain every digest. A live pin keeps its ring.
    #[test]
    fn a_gone_pins_ring_is_resolved_not_unanswered() {
        let mut h = HearTracker::default();
        h.note_inject("SunnyMarsh", None, 100);
        h.note_inject("BoldBay", None, 100);
        h.note_inject("CyanCove", None, 100);
        assert_eq!(h.expire(100 + 120, 120).len(), 3, "all three aged out");
        assert_eq!(h.unanswered_pins().len(), 3);

        // Only CyanCove has a pane now; the other two are gone/retired.
        let resolved = h.retain_live(&["CyanCove".to_string()]);
        assert_eq!(
            resolved,
            vec!["BoldBay".to_string(), "SunnyMarsh".to_string()],
            "both pin-less rings are resolved"
        );
        assert_eq!(
            h.unanswered_pins(),
            vec!["CyanCove".to_string()],
            "the live pin keeps its ring: it can still answer"
        );
    }

    /// The same rule applies to a ring still PENDING: a pin whose pane vanished
    /// between inject and expiry is resolved at once, not left to age.
    #[test]
    fn a_pending_ring_for_a_gone_pin_is_resolved_early() {
        let mut h = HearTracker::default();
        h.note_inject("MistyMarsh", None, 100);
        let resolved = h.retain_live(&["CyanCove".to_string()]);
        assert_eq!(resolved, vec!["MistyMarsh".to_string()]);
        assert!(h.unanswered_pins().is_empty(), "it never aged into deaf");
        assert_eq!(
            h.expire(100 + 120, 120),
            Vec::<String>::new(),
            "nothing left to expire"
        );
    }

    #[test]
    fn heard_clears_and_late_pubkey_clears_deaf() {
        let mut h = HearTracker::default();
        h.note_inject("p", Some("NPUB"), 100);
        h.note_heard_pin("p");
        assert!(h.unanswered_pins().is_empty());
        h.note_inject("q", Some("npub"), 100);
        assert_eq!(h.expire(100 + 120, 120), vec!["q".to_string()]);
        h.note_heard_pubkey("NPUB-x");
        h.note_heard_pubkey("npub");
        assert!(h.unanswered_pins().is_empty());
    }
}

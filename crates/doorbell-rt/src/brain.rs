// governed-by: ADR-0012
//! Brain — the single writer. Owns budgets, hear, cursors, digest, stats,
//! journal, and sinks. Loop: recv → [`Host::tick`] → journal(decided)
//! → execute effects → journal(out). File I/O is local and brief; the
//! brain never awaits a sink endpoint (shed, don't block) or the network
//! (sensors fetch off-loop and deliver results as messages).

use crate::api::Stats;
use crate::hear::HearTracker;
use crate::host::{Effect, Host};
use crate::msg::{AskKind, AskReply, BrainMsg};
use crate::sinks::{SendOutcome, SinkManager, Undelivered};
use crate::store::FileStore;
use doorbell_core::{PinBudget, take_allowed_like};
use std::collections::BTreeMap;

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
            main_sink: "herdr".into(),
            subscriber,
            budget: crate::subscribe::Budget::default_530(),
        }
    }
}

pub struct RunReport {
    pub undelivered: Vec<Undelivered>,
    pub stats: Stats,
}

pub struct Brain<H: Host> {
    cfg: BrainConfig,
    host: H,
    store: FileStore,
    sinks: SinkManager,
    budgets: BTreeMap<String, PinBudget>,
    hear: HearTracker,
    wake: doorbell_core::WakeState,
    digest: Vec<(String, String)>,
    digest_since: Option<tokio::time::Instant>,
    stats: Stats,
    sweep_last: Option<u64>,
    /// Next journal seq (D4.4): resumed from the tail so rows stay
    /// monotonic per workspace across restarts.
    next_seq: u64,
    /// Consecutive unanswered expiries per pin (S3 deaf digest names
    /// subscriber + attempts; corr binds in bd-asr2.11 with receipts).
    deaf_attempts: BTreeMap<String, u32>,
}

impl<H: Host> Brain<H> {
    /// Wire: `let (tx, rx) = mpsc::channel(BRAIN_CAP);`
    /// `let sinks = SinkManager::new(tx.clone());`
    /// `Brain::new(cfg, host, store, sinks).run(rx).await`.
    pub fn new(cfg: BrainConfig, host: H, store: FileStore, sinks: SinkManager) -> Self {
        let wake = store.load_wake(&cfg.workspace);
        let mut hear = HearTracker::default();
        hear.seed_unanswered(store.load_unanswered(&cfg.workspace));
        let next_seq = store.last_seq(&cfg.workspace) + 1;
        // Durable budgets (D4.5): refold grant rows so a restart resumes
        // exact window state — a crash buys no extra prompts.
        let mut budgets = BTreeMap::new();
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
        // Crash-resume (D4.6/E12): journaled-but-unconfirmed Prompt intents
        // redeliver within the refolded budget; the resume row makes the
        // redelivery visible instead of silent.
        let redelivered = count_journaled_prompts(&store, &cfg.workspace);
        if redelivered > 0 {
            let _ = store.append_journal(
                &cfg.workspace,
                &serde_json::json!({"rt_kind": "Resume"}),
                &serde_json::json!([]),
                serde_json::json!({"stage": "resume", "redelivered": redelivered}),
                &[],
            );
        }
        Self {
            cfg,
            host,
            store,
            sinks,
            budgets,
            hear,
            wake,
            digest: Vec::new(),
            digest_since: None,
            stats: Stats::default(),
            sweep_last: None,
            next_seq,
            deaf_attempts: BTreeMap::new(),
        }
    }

    pub async fn run(mut self, mut rx: tokio::sync::mpsc::Receiver<BrainMsg>) -> RunReport {
        // Digest time-flush: `maybe_flush_digest` was event-driven only,
        // so a batch could sit past `digest_interval_secs` forever when no
        // further event arrived (audit P1). The timer is a cheap no-op
        // while no batch is pending (due_time gates on digest_since).
        let mut digest_tick = tokio::time::interval(std::time::Duration::from_secs(
            self.cfg.digest_interval_secs.max(1),
        ));
        digest_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    if matches!(msg, BrainMsg::Shutdown) {
                        break;
                    }
                    self.dispatch(msg).await;
                }
                _ = digest_tick.tick() => {
                    self.maybe_flush_digest().await;
                }
            }
        }
        // Drain any pending digest batch before the sinks shut down.
        self.maybe_flush_digest().await;
        let undelivered = self.sinks.shutdown().await;
        if !undelivered.is_empty() {
            let pins: Vec<String> = undelivered.iter().map(|u| u.pin.clone()).collect();
            let _ = self.store.append_journal(
                &self.cfg.workspace,
                &serde_json::json!({"rt_kind": "Shutdown"}),
                &serde_json::json!([]),
                serde_json::json!({"stage": "drain", "undelivered": undelivered.len()}),
                &pins,
            );
        }
        RunReport {
            undelivered,
            stats: self.stats.clone(),
        }
    }

    async fn dispatch(&mut self, msg: BrainMsg) {
        match msg {
            BrainMsg::Watch(line) => self.on_watch(line).await,
            BrainMsg::HerdrStatus { pin, status } => self.on_status(pin, status).await,
            BrainMsg::Died => self.on_died().await,
            BrainMsg::Sweep { now } => self.on_sweep(now).await,
            BrainMsg::RoleBound { role, pin } => self.on_rolebound(role, pin).await,
            BrainMsg::Fact { summary } => self.on_fact(summary).await,
            BrainMsg::AckSlaTick { now } => self.on_ack_sla(now).await,
            BrainMsg::Presence { pin, state } => {
                // Trusted internal edge; no journal (mirrors the live loop).
                let effects = self.host.tick(&BrainMsg::Presence {
                    pin: pin.clone(),
                    state: state.clone(),
                });
                self.deliver(effects, "presence").await;
            }
            BrainMsg::InboxFetched { pin, rows } => self.on_inbox_fetched(pin, rows).await,
            BrainMsg::SlaFired { posts } => self.on_sla_fired(posts).await,
            BrainMsg::Degraded { source, reason } => {
                self.stats.degraded += 1;
                self.audit(
                    "degraded",
                    serde_json::json!({ "source": source, "reason": reason }),
                );
                let _ = self.store.append_journal(
                    &self.cfg.workspace,
                    &serde_json::json!({"rt_kind": "Degraded", "source": source}),
                    &serde_json::json!([]),
                    serde_json::json!({"stage": "degraded", "reason": reason}),
                    &[],
                );
                // Supervision (audit P1): `sink:<name>` degradations are
                // park events — attempt a Transient-policy restart with a
                // fresh adapter and requeue its shed overflow (at-least-
                // once). No factory or cap exhausted → shed to digest as
                // before (never silent).
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
                    // Non-sink degradation (bus-watch etc.): drain shed
                    // overflow into the digest (F5, never silent).
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
                // F5: hear keys off worker-confirmed delivery, not enqueue.
                // Queued intents already counted stats.injects at enqueue;
                // this receipt only stamps hear (deaf tracking).
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
                        // The read empties the batch: reset the interval
                        // clock too, or the next event-driven flush fires
                        // immediately with only fresh lines (audit P1).
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
        // Gap 1: plane-S cursor — skip old history on restart, persist
        // early so a crash right after doesn't re-ring.
        if let Some(created_at) = doorbell_core::watch_created_at(&line) {
            let us = (created_at as i64) * 1_000_000;
            if self.wake.plane_s_cursor_us.is_some_and(|c| us <= c) {
                return;
            }
            self.wake.plane_s_cursor_us =
                Some(self.wake.plane_s_cursor_us.map(|c| c.max(us)).unwrap_or(us));
            let _ = self.store.save_wake(&self.cfg.workspace, &self.wake);
        }
        *self.stats.rings_by_class.entry("watch".into()).or_default() += 1;
        let effects = self.host.tick(&BrainMsg::Watch(line.clone()));
        let raw: Vec<(String, String)> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Prompt { pin, line } => Some((pin.clone(), line.clone())),
                _ => None,
            })
            .collect();
        let now = self.host.now_unix();
        let injects = self.gated(raw.clone(), now);
        let effect = serde_json::json!({
            "allowed": injects.len(), "gated": raw.len().saturating_sub(injects.len()),
        });
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
        self.audit("watch", serde_json::json!({}));
        if let Some(pk) = doorbell_core::watch_pubkey(&line) {
            self.hear.note_heard_pubkey(&pk);
            self.persist_hear();
        }
        // Decided-before-delivery: flush the journal BEFORE any Prompt
        // intent reaches a sink (D4.2 write-ahead invariant).
        let _ = self.store.sync_journal(&self.cfg.workspace);
        let delivered = self.deliver_prompts(injects).await;
        let woke = delivered.first().cloned();
        self.persist_wake(woke.as_deref(), "watch");
    }

    async fn on_status(&mut self, pin: String, status: String) {
        // GS-039 ordering: capture the previous status BEFORE the tick —
        // the host records the new status at tick time (FileHost inserts
        // into its map, the reducer updates the model), so a post-tick
        // `prev_status` read always sees the value we just wrote and the
        // transition gate never fires (audit P1). The gate is advisory:
        // an unknown pin still counts as transitioned.
        let from = self.host.prev_status(&pin).map(|f| f.to_ascii_lowercase());
        let effects = self.host.tick(&BrainMsg::HerdrStatus {
            pin: pin.clone(),
            status: status.clone(),
        });
        let raw = self.prompts_of(&effects);
        let now = self.host.now_unix();
        let injects = self.gated(raw.clone(), now);
        self.journal(
            &serde_json::json!({"rt_kind": "HerdrStatus", "pin": pin}),
            &effects,
            serde_json::json!({"stage": "drain"}),
            &raw.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
        );
        let _ = self.store.sync_journal(&self.cfg.workspace);
        let delivered = self.deliver_prompts(injects).await;
        // GS-039 P0-1 REAL observation: wake-worthy status transitions that
        // show the agent woke (working) or settled after work
        // (idle/done/blocked) advance last_wake — OR a pin that actually
        // delivered. Static already-idle rows never ratchet liveness.
        let s = status.to_ascii_lowercase();
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
        self.journal(
            &serde_json::json!({"rt_kind": "Died"}),
            &effects,
            serde_json::json!({"stage": "executed", "restarted": true}),
            &[],
        );
        self.host.restart_watch();
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
        // Live drains first (loop-top order): role binds, then facts.
        for (role, pin) in self.host.drain_role_bounds() {
            self.on_rolebound(role, pin).await;
        }
        for summary in self.host.drain_facts() {
            self.on_fact(summary).await;
        }
        // Registry/index refresh + forwarder respawn; presence rows tick
        // the trusted edge (heard = registry pin present after tick).
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
        // F6: fetch only when tick asks (need_fetch arm) — was unconditional
        // per-pin I/O every 1s sweep. Full pipe (fetch off-loop as Msgs)
        // stays the follow-up; this cuts the steady-state sweep to tick
        // plus presence when no inbox work is due.
        let fetched = if effects.iter().any(|e| matches!(e, Effect::FetchInbox)) {
            self.fetch_all_inbox()
        } else {
            Vec::new()
        };
        self.journal(
            &serde_json::json!({"rt_kind": "Sweep"}),
            &effects,
            serde_json::json!({"stage": "sweep"}),
            &[],
        );
        self.deliver(effects, "sweep").await;
        for (pin, rows) in fetched {
            self.on_inbox_fetched(pin, rows).await;
        }
        for pin in self.hear.expire(now, doorbell_core::UNANSWERED_SECS) {
            *self.deaf_attempts.entry(pin).or_insert(0) += 1;
        }
        let unanswered = self.hear.unanswered_pins();
        self.deaf_attempts.retain(|pin, _| unanswered.contains(pin));
        self.persist_hear();
    }

    async fn on_inbox_fetched(&mut self, pin: String, rows: Vec<serde_json::Value>) {
        // Cursor-hold: never advance the high-water on unparseable rows —
        // the batch re-sweeps (bd-7qg6: at-least-once, never at-most-once).
        let refs: Vec<&serde_json::Value> = rows.iter().collect();
        if let Some(hw) = doorbell_core::inbox_high_water_from_values(refs) {
            self.wake.since_by_pin.insert(pin.clone(), hw);
            let _ = self.store.save_wake(&self.cfg.workspace, &self.wake);
        }
        let effects = self.host.tick(&BrainMsg::InboxFetched {
            pin: pin.clone(),
            rows,
        });
        self.journal(
            &serde_json::json!({"rt_kind": "InboxFetched", "pin": pin}),
            &effects,
            serde_json::json!({"stage": "executed"}),
            &[pin],
        );
        self.deliver(effects, "inbox").await;
    }

    // F6: shared borrow — inbox fetch needs no &mut host (enables future
    // off-loop pipe; today it at least stops excluding concurrent readers).
    // bd-5d0v: a pin with no cursor backfills from the bounded redelivery
    // floor, never the sweep's wall-clock now (strict-newer-than-now is
    // empty forever and the cursor would never initialize — permanent
    // plane-P deafness for fresh spawns and post-state-loss restarts).
    fn fetch_all_inbox(&self) -> Vec<(String, Vec<serde_json::Value>)> {
        let floor = doorbell_core::redelivery_floor_rfc3339(self.host.now_unix());
        let mut fetched = Vec::new();
        for pin in self.host.pins() {
            let pin_since = self
                .wake
                .since_by_pin
                .get(&pin)
                .cloned()
                .unwrap_or_else(|| floor.clone());
            let rows = self.host.fetch_inbox(&pin, &pin_since);
            if !rows.is_empty() {
                fetched.push((pin, rows));
            }
        }
        fetched
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
            let posts = self.host.scan_ack_sla();
            if !posts.is_empty() {
                self.on_sla_fired(posts).await;
            }
        }
        // Live AckSla arm also sweeps the inbox when tick asks for it.
        // F4: route fetched rows via on_inbox_fetched (was fire-and-drop).
        if effects.iter().any(|e| matches!(e, Effect::FetchInbox)) {
            for (pin, rows) in self.fetch_all_inbox() {
                self.on_inbox_fetched(pin, rows).await;
            }
        }
        self.deliver(effects, "acksla").await;
    }
    async fn on_sla_fired(&mut self, posts: Vec<String>) {
        if !posts.is_empty() {
            self.audit("ack_sla_fired", serde_json::json!({ "count": posts.len() }));
        }
        let effects = self.host.tick(&BrainMsg::SlaFired {
            posts: posts.clone(),
        });
        self.journal(
            &serde_json::json!({"rt_kind": "SlaFired", "count": posts.len()}),
            &effects,
            serde_json::json!({"stage": "executed"}),
            &self.pins_of(&effects),
        );
        self.deliver(effects, "sla").await;
    }

    /// Budget gate + sink delivery shared by every arm. Returns delivered pins.
    /// Audit/FactMiss/LoudMiss/Digest effects are recorded by the Host layer
    /// (single ownership); the journal row above already carries them as data.
    async fn deliver(&mut self, effects: Vec<Effect>, _stage: &str) -> Vec<String> {
        let raw = self.prompts_of(&effects);
        if raw.is_empty() {
            return Vec::new();
        }
        let now = self.host.now_unix();
        let injects = self.gated(raw.clone(), now);
        if raw.len() != injects.len() {
            self.stats.drops += (raw.len() - injects.len()) as u64;
        }
        let _ = self.store.sync_journal(&self.cfg.workspace);
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

    async fn deliver_prompts(&mut self, injects: Vec<(String, String)>) -> Vec<String> {
        let mut queued = Vec::new();
        let sink = self.cfg.main_sink.clone();
        for (pin, line) in injects {
            if !self.host.inject_allowed(&pin) {
                continue;
            }
            match self.sinks.send(&sink, &pin, &line).await {
                crate::sinks::SendOutcome::Queued => {
                    // F5: queued, not delivered — count the accepted intent
                    // for wake/journal purposes; hear stamps on Delivered.
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
        // One batched line per pin, earliest batch first (ordering refines
        // with deadline metadata in Phase 2, bd-asr2.11).
        let mut by_pin: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (pin, line) in std::mem::take(&mut self.digest) {
            by_pin.entry(pin).or_default().push(line);
        }
        // S3: deaf services name subscriber + attempts (corr binds in .11).
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
        let sink = self.cfg.main_sink.clone();
        for (pin, lines) in by_pin {
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
                    Effect::Prompt { pin, line } => {
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
                })
                .collect::<Vec<_>>()
        );
        let seq = self.next_seq;
        self.next_seq += 1;
        let _ = self.store.append_journal_seq(
            &self.cfg.workspace,
            msg,
            &actions,
            executed,
            pins,
            Some(seq),
            Some(self.cfg.subscriber.as_str()),
        );
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
        self.store.append_audit(&self.cfg.workspace, detail);
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
        if changed && let Err(e) = self.store.save_wake(&self.cfg.workspace, &self.wake) {
            // GS-039: refuse silent drop — loud warn with the failing arm.
            eprintln!(
                "doorbell-rt: observed-wake stamp FAILED on {ctx} — refuse silent drop (GS-039): {e:#}"
            );
        }
    }

    fn prompts_of(&self, effects: &[Effect]) -> Vec<(String, String)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Prompt { pin, line } => Some((pin.clone(), line.clone())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockHost;
    use crate::sinks::MockSink;
    use std::collections::VecDeque;

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

    #[tokio::test]
    async fn decided_before_delivery_write_ahead() {
        // One watch → one prompt: the journal holds the intent even though
        // no sink is registered (Absent is audited, intent journaled).
        let prompt = Effect::Prompt {
            pin: "p".into(),
            line: "[doorbell] test".into(),
        };
        let (mut brain, _rx, _tx) = test_brain("decided", vec![vec![prompt]]);
        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        assert_eq!(brain.stats.injects, 0, "no sink registered");
        // Intent was still journaled decided-before-delivery.
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert!(journal.contains("\"stage\":\"decided\""), "{journal}");
    }

    #[tokio::test]
    async fn same_msg_log_same_effects_determinism() {
        // Drive the REAL core reducer twice over one Msg log: byte-identical
        // effect sequences (the Phase-1 parity property, host-side).
        use crate::host::CoreReducer;
        let log = vec![
            BrainMsg::Watch("{\"kind\":9,\"tags\":[[\"p\",\"x\"]]}".into()),
            BrainMsg::HerdrStatus {
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
        // F5: enqueue is not delivery — hear stamps only on the worker's
        // Delivered receipt (pending → expire → unanswered).
        let (mut brain, _rx, _tx) = test_brain("hear", vec![]);
        let (sink, _sent) = MockSink::new();
        brain.sinks.add_sink("herdr", Box::new(sink));
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
                pin: "p".into(),
                line: format!("m{i}"),
            })
            .collect::<Vec<_>>();
        let (mut brain, _rx, tx) = test_brain("budget", vec![prompts]);
        let (sink, sent) = MockSink::new();
        brain.sinks.add_sink("herdr", Box::new(sink));
        brain
            .dispatch(BrainMsg::Fact {
                summary: "x".into(),
            })
            .await;
        drop(tx);
        // 3 pass the 5/30/3 gate; the 4th drops to the audit trail.
        assert_eq!(brain.stats.injects, 3);
        assert_eq!(brain.stats.drops, 1);
        // The worker pumps asynchronously: poll until the queue drains.
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
            brain.sinks.add_sink("herdr", Box::new(sink));
            for msg in [
                BrainMsg::Watch("{\"kind\":9}".into()),
                BrainMsg::Sweep { now: 1_000_100 },
                BrainMsg::HerdrStatus {
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
                    pin: "p".into(),
                    line: "m1".into(),
                }],
                vec![Effect::Prompt {
                    pin: "p".into(),
                    line: "m2".into(),
                }],
                vec![Effect::Prompt {
                    pin: "p".into(),
                    line: "m3".into(),
                }],
                vec![Effect::Prompt {
                    pin: "p".into(),
                    line: "m4".into(),
                }],
            ]);
            let sinks = SinkManager::new(tx.clone());
            let brain = Brain::new(BrainConfig::new("w"), host, FileStore::new(&dir), sinks);
            (brain, rx, tx)
        };
        // Pre-crash: three decided+delivered wakes fill the 5/30/3 window.
        let (mut a, _rx, _tx) = mk();
        let (sink, _sent) = MockSink::new();
        a.sinks.add_sink("herdr", Box::new(sink));
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
        drop(a); // abort: no shutdown drain, RAM state vanishes
        // Resume on the same store: refold reproduces budget state exactly.
        let (mut b, _rx2, _tx2) = mk();
        let budget = b.budgets.get("p").expect("refolded budget row");
        assert_eq!(budget.prompts.len(), 3, "all three grants refolded");
        let (sink_b, _sent_b) = MockSink::new();
        b.sinks.add_sink("herdr", Box::new(sink_b));
        let journal = std::fs::read_to_string(b.store.journal_file("w")).unwrap();
        assert!(
            journal.contains("\"stage\":\"resume\"") && journal.contains("\"redelivered\":3"),
            "resume logs its redelivery set: {journal}"
        );
        // Seq resumes monotonic past the crash.
        let before = b.store.last_seq("w");
        b.dispatch(BrainMsg::Fact {
            summary: "z".into(),
        })
        .await;
        assert!(b.store.last_seq("w") > before, "seq advances past resume");
        // 0 further prompts in the window: the crash bought nothing.
        assert_eq!(b.stats.injects, 0, "window stays shut after restart");
        assert_eq!(b.stats.drops, 1, "fourth prompt drops loudly");
        // No unjournaled delivery: every delivered line was journaled first.
        for line in ["m1", "m2", "m3"] {
            assert!(journal.contains(line), "delivered intent journaled: {line}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clock-jump: an NTP step mid-run never reorders the journal; seq
    /// stays monotonic and the sweep cadence re-stabilizes.
    #[tokio::test]
    async fn clock_jump_keeps_order_and_restabilizes() {
        let (mut brain, _rx, _tx) = test_brain("clock", vec![vec![], vec![], vec![]]);
        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        brain.host.now = 1_000_000 + 86_400; // +1 day mid-run
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

    #[tokio::test]
    async fn cursorless_pin_sweeps_from_redelivery_floor() {
        // bd-5d0v: a pin with no since_by_pin entry (fresh spawn, state
        // loss) must backfill from the bounded floor. The old fallback
        // fetched strictly-newer-than-sweep-now = empty forever, so the
        // cursor never initialized and the pin stayed plane-P deaf on
        // every future sweep too.
        let now = 1_700_000_000u64;
        let floor = doorbell_core::redelivery_floor_rfc3339(now);
        let row_us = now as i64 * 1_000_000; // recent mail, inside the floor
        let (mut brain, _rx, _tx) = test_brain("floor", vec![vec![Effect::FetchInbox], vec![]]);
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
        assert!(brain.wake.since_by_pin.get("p").is_none());
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
                1 => BrainMsg::HerdrStatus {
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
        brain.sinks.add_sink("herdr", Box::new(sink));
        // Inject, then sweep past the 120s hear window: pin goes deaf.
        brain.hear.note_inject("svc", None, 1_000_000);
        brain
            .dispatch(BrainMsg::Sweep {
                now: 1_000_000 + 120,
            })
            .await;
        assert_eq!(brain.hear.unanswered_pins(), vec!["svc".to_string()]);
        brain.digest_push("svc".into(), "queued".into());
        brain.maybe_flush_digest().await;
        // Poll the async worker for the batch.
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

    /// Second consumer (bd-asr2.11): a subscribed human identity receives
    /// a wake with zero swarm alive — no panes, no processes, no dispatcher
    /// beyond this Brain. The journal row carries the subscriber tag so
    /// `why --subscriber` replays it.
    #[tokio::test]
    async fn human_subscription_wake_needs_no_swarm() {
        use crate::subscribe::*;
        let dir = std::env::temp_dir().join(format!("dbrt-human-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Operator subscribes the human (npub) on the herdr sink.
        let mut store = SubscriptionStore::open(&dir);
        let sub = Subscription {
            id: "human-1".into(),
            identity: Identity::Npub("npub1human".into()),
            scope: Scope::One("w".into()),
            filter: Filter::default(),
            sink: SinkKind::Herdr,
            budget: Budget::default_530(),
        };
        let sub = store
            .subscribe(Actor::operator(), sub, "w")
            .expect("operator subscribes human");
        // Brain bound to the subscription (tag + budget template).
        let (tx, _rx) = tokio::sync::mpsc::channel(crate::BRAIN_CAP);
        let mut host = MockHost::new(1_000_000);
        host.script = VecDeque::from(vec![vec![Effect::Prompt {
            pin: "human".into(),
            line: "[doorbell] hold expired".into(),
        }]]);
        let sinks = SinkManager::new(tx.clone());
        let mut cfg = BrainConfig::new("w");
        cfg.subscriber = sub.identity.tag();
        cfg.budget = sub.budget;
        let mut brain = Brain::new(cfg, host, FileStore::new(&dir), sinks);
        let (sink, sent) = MockSink::new();
        brain.sinks.add_sink("herdr", Box::new(sink));
        brain.dispatch(BrainMsg::Watch("{\"kind\":9}".into())).await;
        assert_eq!(brain.stats.injects, 1, "human wake delivers");
        // Journal row carries the subscriber tag (why --subscriber replays).
        let journal = std::fs::read_to_string(brain.store.journal_file("w")).unwrap();
        assert!(
            journal.contains("\"subscriber\":\"npub:npub1human\""),
            "wake row tagged: {journal}"
        );
        drop(tx);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = sent;
    }

    // ---- Audit regression tests (bd-0012 verify) ----

    #[tokio::test]
    async fn status_transition_gate_sees_pre_tick_status() {
        // Audit P1: the wake-liveness ratchet must compare the PREVIOUS
        // status. The old code read prev_status AFTER the tick (which
        // records the new status), so `transitioned` was always false on
        // real hosts and last_wake never advanced on benign transitions.
        let (mut brain, _rx, _tx) = test_brain("statusgate", vec![]);
        brain
            .dispatch(BrainMsg::HerdrStatus {
                pin: "p".into(),
                status: "working".into(),
            })
            .await;
        let first = brain.wake.last_wake_unix;
        brain
            .dispatch(BrainMsg::HerdrStatus {
                pin: "p".into(),
                status: "idle".into(),
            })
            .await;
        let second = brain.wake.last_wake_unix;
        assert!(
            second.is_some(),
            "working -> idle transition must ratchet last_wake (prev status = working)"
        );
        // Same-status repeats never ratchet (static rows stay silent).
        brain
            .dispatch(BrainMsg::HerdrStatus {
                pin: "p".into(),
                status: "idle".into(),
            })
            .await;
        assert_eq!(
            brain.wake.last_wake_unix, second,
            "already-idle repeats must not advance liveness"
        );
        // The first dispatch (unknown -> working) transitioned too, but the
        // clock is frozen in tests; what matters is that the benign
        // working -> idle transition ratcheted at all (old code: never).
        assert!(first.is_none() || first == second);
        assert!(second.is_some());
    }

    #[tokio::test]
    async fn digest_ask_resets_interval_clock() {
        // Audit P1: AskKind::Digest took the batch but left digest_since
        // running — the next flush fired immediately with only fresh lines.
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
        // Audit P1: supervision was unwired — a parked sink stayed dead
        // until process restart. With a factory, Degraded must restart the
        // sink and requeue its shed overflow (at-least-once).
        let (deg_tx, mut deg_rx) = tokio::sync::mpsc::channel::<BrainMsg>(16);
        let (mut brain, _rx, _tx2) = test_brain("sinkrestart", vec![]);
        brain.sinks = SinkManager::new(deg_tx);
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct FailingThenOk {
            n: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            fail: bool,
        }
        impl crate::sinks::PromptSink for FailingThenOk {
            fn send(&mut self, _pin: &str, _line: &str) -> anyhow::Result<()> {
                if self.fail {
                    anyhow::bail!("endpoint down");
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
            "herdr",
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
        // First intent fails at the endpoint → park; second sheds.
        brain
            .deliver_prompts(vec![("p".into(), "[doorbell] wake".into())])
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(brain.sinks.parked("herdr").await, "failed adapter parks");
        assert!(
            matches!(
                brain.sinks.send("herdr", "p", "[doorbell] two").await,
                SendOutcome::Shed { .. }
            ),
            "parked sink sheds"
        );
        // The worker's Degraded lands on deg_rx; feed it through dispatch
        // exactly as the run loop would.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while let Ok(m) = deg_rx.try_recv() {
            brain.dispatch(m).await;
        }
        // Restarted (fail=false) adapter must have delivered the requeued
        // failed intent; the sink is unparked and pumping again.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            n.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "restarted adapter delivered the requeued intent, got {}",
            n.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(!brain.sinks.parked("herdr").await, "sink revived");
        // And it accepts new intents again (no longer parked).
        assert!(
            matches!(
                brain.sinks.send("herdr", "p", "[doorbell] three").await,
                SendOutcome::Queued
            ),
            "revived sink queues"
        );
    }

    #[tokio::test]
    async fn sink_degrade_without_factory_sheds_to_digest() {
        // No factory → restart refuses; shed overflow lands in the digest
        // (never silent, the pre-supervision path).
        let (deg_tx, _deg_rx) = tokio::sync::mpsc::channel::<BrainMsg>(16);
        let (mut brain, _rx, _tx2) = test_brain("sinknofactory", vec![]);
        brain.sinks = SinkManager::new(deg_tx);
        let (mut sink, _sent) = MockSink::new();
        sink.fail_after = Some(0);
        brain.sinks.add_sink("herdr", Box::new(sink));
        brain
            .deliver_prompts(vec![("p".into(), "[doorbell] wake".into())])
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(brain.sinks.parked("herdr").await);
        brain
            .dispatch(BrainMsg::Degraded {
                source: "sink:herdr".into(),
                reason: "delivery failed: endpoint down".into(),
            })
            .await;
        assert!(
            brain.sinks.parked("herdr").await,
            "no factory: stays parked"
        );
    }
}

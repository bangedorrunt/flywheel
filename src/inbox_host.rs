// governed-by: ADR-0041, ADR-0040
//! Phase 3: the doorbell's recipient-side wake, hosted (build-plan §6).
//!
//! `toron_core::inbox_program::InboxProgram` is the one pure fold that owns
//! head, cursor, binding and ring bookkeeping per recipient; every ring
//! decision is `toron_core::wake::should_ring`. This module is the host the
//! build plan asks for: the fold runs on ONE dedicated OS thread (never
//! inside a Tokio runtime, so it can never nest a `block_on`), its inputs
//! arrive through a channel, and every effect it asks for goes through the
//! [`Ports`] boundary:
//!
//! ```text
//!   durable facts + herdr --observe--> msgs --InboxProgram::update--> cmds
//!   cmds --Ports--> pane write (pane id + binding gen) / reverify / facts / notify
//! ```
//!
//! Pure here: [`Core`] (translate an [`Observation`] into fold messages,
//! step the fold, derive the `bind` facts). Effects here: only
//! [`execute`], which calls [`Ports`]. The fold never reads a clock, the
//! store or herdr; time arrives as `now_ms`.
//!
//! A ring is never delivery: `rings_since_advance` resets only on a `Cursor`
//! fact the host reads from the log (the recipient ran `toron next`). At the
//! ring budget the fold answers with the `deaf{r, wm}` fact, a supervisor
//! notification and a captain notification, once per watermark, and stops
//! ringing.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use toron_core::inbox_program::{
    DeafCause, InboxCmd, InboxModel, InboxMsg, InboxProgram, Invalidate,
};
use toron_core::kernel::{Cmd as _, Durability, Program};
use toron_core::wake::{PaneState, Urgency};

/// A binding as verified by the host this round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub pane_id: String,
    pub herdr_session: String,
    pub verified: bool,
    pub verified_by: String,
}

/// One recipient as observed this round: durable log facts plus herdr's view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub npub: String,
    pub pin: String,
    pub head: u64,
    pub cursor: u64,
    pub urgency: Urgency,
    pub pane: PaneState,
    pub bind: Option<Bind>,
}

/// Everything read this round.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    pub seen: Vec<Seen>,
    /// herdr was unreachable and is reachable again: pane ids may not be the
    /// ones verified (invalidates every binding).
    pub herdr_restarted: bool,
}

/// The effect boundary. Production: `inbox_ports::FlywheelPorts`; tests:
/// a recorder.
pub trait Ports: Send + 'static {
    /// Read head, cursor, pane state and the binding for every recipient.
    fn observe(&mut self) -> Observation;
    /// Re-run binding verification (env + `report_metadata`) for one npub.
    fn reverify(&mut self, npub: &str) -> Option<Bind>;
    /// Type the constant wake line into `pane_id`. The host has already
    /// checked `binding_gen` against the fold's generation.
    fn ring(&mut self, npub: &str, pane_id: &str, wm: u64, line: &str) -> Result<(), String>;
    /// Record one durable, keyed fact (`bind`, `deaf`). Idempotent by key.
    fn fact(&mut self, key: &str, fact: Value) -> Result<(), String>;
    /// A notify envelope for the supervisor's inbox.
    fn notify_supervisor(&mut self, npub: &str, wm: u64, cause: DeafCause) -> Result<(), String>;
    /// A notification for the captain.
    fn notify_captain(&mut self, npub: &str, wm: u64, cause: DeafCause) -> Result<(), String>;
    /// The ring budgets a previous doorbell process left behind (empty when
    /// none). Read once, at host start.
    fn load_budget(&mut self) -> BTreeMap<String, Budget> {
        BTreeMap::new()
    }
    /// Persist the ring budgets before the rings they paid for go out.
    fn save_budget(&mut self, _budget: &BTreeMap<String, Budget>) -> Result<(), String> {
        Ok(())
    }
}

/// The ring-budget state of one recipient that must survive a doorbell
/// restart (follow-up b). The fold's model is RAM-only; without this a
/// restart gave every deaf recipient a fresh budget (an unbounded ring
/// total over repeated restarts) and re-emitted `deaf`. Time is not kept:
/// `now_ms` is relative to the process start, so a restored recipient has no
/// last-ring time and the first tick decides from the budget alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub head: u64,
    pub cursor: u64,
    pub last_ring_wm: Option<u64>,
    pub rings_since_advance: u32,
    pub deaf: Option<u64>,
    /// The pane the budget was spent on. A restored binding is UNVERIFIED and
    /// is re-verified before any ring; a different pane is a new incarnation
    /// and gets a fresh budget (the fold's own rule).
    pub pane_id: Option<String>,
    pub herdr_session: Option<String>,
}

/// First wait after a failed re-verify, doubling to [`REVERIFY_BACKOFF_CAP_MS`]
/// (follow-up c). Fold-clock milliseconds.
pub const REVERIFY_BACKOFF_BASE_MS: u64 = 2_000;
/// The longest wait between re-verifies of one unbound recipient.
pub const REVERIFY_BACKOFF_CAP_MS: u64 = 60_000;

/// Pure: the wait after the `fails`-th consecutive failed re-verify.
pub fn reverify_backoff_ms(fails: u32) -> u64 {
    REVERIFY_BACKOFF_BASE_MS
        .saturating_mul(1u64 << fails.saturating_sub(1).min(5))
        .min(REVERIFY_BACKOFF_CAP_MS)
}

/// An input to the host thread besides the periodic tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Look at one recipient now (a lost hint costs nothing: the tick reads
    /// the same facts).
    Hint(String),
    /// The generation moves: every binding goes stale and is re-verified
    /// before any ring.
    Invalidate(Invalidate),
}

/// Pure core: the fold's model plus the translation of what was observed.
#[derive(Clone, Debug, Default)]
pub struct Core {
    pub model: InboxModel,
    /// This round's clock (the fold's `now_ms`), for the re-verify backoff.
    pub now_ms: u64,
    /// Consecutive failed re-verifies per recipient.
    reverify_fails: BTreeMap<String, u32>,
    /// Earliest `now_ms` of the next re-verify per recipient.
    reverify_next_ms: BTreeMap<String, u64>,
    /// What was last persisted, so an unchanged budget writes nothing.
    saved: BTreeMap<String, Budget>,
}

fn tag(cause: DeafCause) -> &'static str {
    match cause {
        DeafCause::NoAnswer => "no_answer",
        DeafCause::Unbound => "unbound",
    }
}

impl Core {
    /// Pure: the budgets worth persisting (recipients that have spent rings
    /// or been declared deaf).
    pub fn snapshot(&self) -> BTreeMap<String, Budget> {
        self.model
            .recipients
            .iter()
            .filter(|(_, r)| r.rings_since_advance > 0 || r.deaf.is_some())
            .map(|(npub, r)| {
                (
                    npub.clone(),
                    Budget {
                        head: r.head,
                        cursor: r.cursor,
                        last_ring_wm: r.last_ring_wm,
                        rings_since_advance: r.rings_since_advance,
                        deaf: r.deaf,
                        pane_id: r.binding.as_ref().map(|b| b.pane_id.clone()),
                        herdr_session: r.binding.as_ref().map(|b| b.herdr_session.clone()),
                    },
                )
            })
            .collect()
    }

    /// Seed the model from persisted budgets (host start, before any round).
    pub fn restore(&mut self, budgets: BTreeMap<String, Budget>) {
        for (npub, b) in &budgets {
            let r = self.model.recipients.entry(npub.clone()).or_default();
            r.head = b.head;
            r.cursor = b.cursor;
            r.last_ring_wm = b.last_ring_wm;
            r.rings_since_advance = b.rings_since_advance;
            r.deaf = b.deaf;
            r.binding = b
                .pane_id
                .as_ref()
                .map(|pane_id| toron_core::inbox_program::Binding {
                    pane_id: pane_id.clone(),
                    herdr_session: b.herdr_session.clone().unwrap_or_default(),
                    generation: self.model.generation,
                    verified: false,
                    verified_by: "restored".into(),
                });
        }
        self.saved = budgets;
    }

    /// Fold messages in order; return the commands they asked for.
    pub fn fold(&mut self, msgs: &[InboxMsg]) -> Vec<InboxCmd> {
        let mut out = Vec::new();
        for msg in msgs {
            let u = InboxProgram::update(&self.model, msg);
            self.model = u.model;
            out.extend(u.commands);
        }
        out
    }

    /// Pure: observed state to fold messages. A binding is offered only when
    /// the model has none, or when the pane differs (a new incarnation); a
    /// stale binding (after an invalidation) is re-established only through
    /// the fold's own `Reverify` command, never silently here.
    pub fn msgs_for(&self, obs: &Observation) -> Vec<InboxMsg> {
        let mut m = Vec::new();
        if obs.herdr_restarted {
            m.push(InboxMsg::Invalidate(Invalidate::HerdrRestarted));
        }
        let generation = self.model.generation + u64::from(obs.herdr_restarted);
        for s in &obs.seen {
            m.push(InboxMsg::Head {
                recipient: s.npub.clone(),
                head: s.head,
                urgency: s.urgency,
            });
            m.push(InboxMsg::Cursor {
                recipient: s.npub.clone(),
                cursor: s.cursor,
            });
            m.push(InboxMsg::Pane {
                recipient: s.npub.clone(),
                state: s.pane,
            });
            if let Some(b) = &s.bind {
                let have = self
                    .model
                    .recipients
                    .get(&s.npub)
                    .and_then(|r| r.binding.as_ref());
                let offer = have.is_none_or(|h| h.pane_id != b.pane_id);
                if offer {
                    m.push(bound_msg(&s.npub, b, generation));
                }
            }
        }
        m
    }
}

fn bound_msg(npub: &str, b: &Bind, generation: u64) -> InboxMsg {
    InboxMsg::Bound {
        recipient: npub.to_string(),
        pane_id: b.pane_id.clone(),
        herdr_session: b.herdr_session.clone(),
        generation,
        verified: b.verified,
        verified_by: b.verified_by.clone(),
    }
}

/// Pure: the `bind{npub, pane_id, herdr_session, gen, verified_by}` facts
/// for every binding that changed between two models. Keyed so a replay
/// writes each once.
pub fn bind_facts(before: &InboxModel, after: &InboxModel) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (npub, r) in &after.recipients {
        let Some(b) = &r.binding else { continue };
        let was = before.recipients.get(npub).and_then(|r| r.binding.as_ref());
        if was == Some(b) {
            continue;
        }
        out.push((
            format!("bind:{npub}:{}:{}:{}", b.pane_id, b.generation, b.verified),
            json!({
                "kind": "bind",
                "npub": npub,
                "pane_id": b.pane_id,
                "herdr_session": b.herdr_session,
                "gen": b.generation,
                "verified": b.verified,
                "verified_by": b.verified_by,
            }),
        ));
    }
    out
}

/// What one round did, for the status counters and the tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoundReport {
    pub rings: u64,
    pub deaf: u64,
    pub binds: u64,
    pub failed: u64,
    pub reverified: u64,
    /// Re-verifies skipped because the recipient is inside its backoff.
    pub backed_off: u64,
}

fn write_binds(core: &Core, before: &InboxModel, ports: &mut dyn Ports, rep: &mut RoundReport) {
    for (key, fact) in bind_facts(before, &core.model) {
        match ports.fact(&key, fact) {
            Ok(()) => rep.binds += 1,
            Err(e) => eprintln!("flywheel inbox: bind fact {key} not written: {e}"),
        }
    }
}

/// The effect boundary: interpret commands through `ports`. A failed command
/// is fed back as `CmdFailed` (it changes nothing; the next tick decides
/// again from durable state).
pub fn execute(core: &mut Core, ports: &mut dyn Ports, cmds: Vec<InboxCmd>, rep: &mut RoundReport) {
    for cmd in cmds {
        let key = match cmd.durability() {
            Durability::Durable { key } => Some(key),
            _ => None,
        };
        let res: Result<(), String> = match cmd {
            InboxCmd::Ring {
                recipient,
                pane_id,
                binding_gen,
                wm,
                line,
                ..
            } => {
                // by pane id and binding generation, never by name: a ring
                // for a generation the fold has since invalidated is dropped.
                if binding_gen != core.model.generation {
                    Err(format!(
                        "ring for gen {binding_gen} dropped: fold is at gen {}",
                        core.model.generation
                    ))
                } else {
                    let r = ports.ring(&recipient, &pane_id, wm, &line);
                    if r.is_ok() {
                        rep.rings += 1;
                    }
                    r
                }
            }
            InboxCmd::Reverify { recipient } => {
                // Follow-up c: an unbound recipient is re-verified at a
                // backed-off pace, not a `pane.list` per tick. Escalation
                // counts ticks, so it is not delayed by this.
                let due = core
                    .reverify_next_ms
                    .get(&recipient)
                    .is_none_or(|t| core.now_ms >= *t);
                if !due {
                    rep.backed_off += 1;
                } else {
                    rep.reverified += 1;
                    let mut verified = false;
                    if let Some(b) = ports.reverify(&recipient) {
                        verified = b.verified;
                        let before = core.model.clone();
                        let g = core.model.generation;
                        core.fold(&[bound_msg(&recipient, &b, g)]);
                        write_binds(core, &before, ports, rep);
                    }
                    if verified {
                        core.reverify_fails.remove(&recipient);
                        core.reverify_next_ms.remove(&recipient);
                    } else {
                        let f = core.reverify_fails.entry(recipient.clone()).or_default();
                        *f = f.saturating_add(1);
                        let wait = reverify_backoff_ms(*f);
                        core.reverify_next_ms
                            .insert(recipient.clone(), core.now_ms.saturating_add(wait));
                    }
                }
                Ok(())
            }
            InboxCmd::RecordDeaf {
                recipient,
                wm,
                cause,
            } => {
                rep.deaf += 1;
                ports.fact(
                    key.as_deref().unwrap_or("deaf"),
                    json!({
                        "kind": "deaf",
                        "npub": recipient,
                        "wm": wm,
                        "cause": tag(cause),
                    }),
                )
            }
            InboxCmd::NotifySupervisor {
                recipient,
                wm,
                cause,
            } => ports.notify_supervisor(&recipient, wm, cause),
            InboxCmd::NotifyCaptain {
                recipient,
                wm,
                cause,
            } => ports.notify_captain(&recipient, wm, cause),
        };
        if let Err(e) = res {
            rep.failed += 1;
            eprintln!("flywheel inbox: command failed: {e}");
            core.fold(&[InboxMsg::CmdFailed(e)]);
        }
    }
}

/// One round: observe, fold, execute. `inputs` are what arrived on the
/// channel since the last round.
pub fn round(
    core: &mut Core,
    ports: &mut dyn Ports,
    inputs: Vec<Input>,
    now_ms: u64,
) -> RoundReport {
    let mut rep = RoundReport::default();
    core.now_ms = now_ms;
    let obs = ports.observe();
    let before = core.model.clone();
    let mut msgs: Vec<InboxMsg> = Vec::new();
    let mut hints = Vec::new();
    for i in inputs {
        match i {
            Input::Invalidate(w) => msgs.push(InboxMsg::Invalidate(w)),
            Input::Hint(n) => hints.push(n),
        }
    }
    // A generation move (or a herdr restart) is a reason to look again now.
    if obs.herdr_restarted || msgs.iter().any(|m| matches!(m, InboxMsg::Invalidate(_))) {
        core.reverify_fails.clear();
        core.reverify_next_ms.clear();
    }
    msgs.extend(core.msgs_for(&obs));
    for n in hints {
        msgs.push(InboxMsg::Hint {
            recipient: n,
            now_ms,
        });
    }
    // The periodic full re-evaluation: level-triggered, so a lost hint or a
    // dead channel costs nothing.
    msgs.push(InboxMsg::Tick { now_ms });
    let cmds = core.fold(&msgs);
    write_binds(core, &before, ports, &mut rep);
    // Follow-up b: the budget a ring spends is persisted BEFORE the ring goes
    // out, so a crash between the two can only under-ring, never exceed it.
    let snap = core.snapshot();
    if snap != core.saved {
        match ports.save_budget(&snap) {
            Ok(()) => core.saved = snap,
            Err(e) => {
                rep.failed += 1;
                eprintln!("flywheel inbox: persisting the ring budget failed: {e}");
            }
        }
    }
    execute(core, ports, cmds, &mut rep);
    rep
}

/// Exact counters, shared with the status surface.
#[derive(Debug, Default)]
pub struct Stats {
    pub rounds: AtomicU64,
    pub rings: AtomicU64,
    pub deaf: AtomicU64,
    pub binds: AtomicU64,
    pub failed: AtomicU64,
    pub reverified: AtomicU64,
}

impl Stats {
    fn add(&self, r: &RoundReport) {
        self.rounds.fetch_add(1, Ordering::Relaxed);
        self.rings.fetch_add(r.rings, Ordering::Relaxed);
        self.deaf.fetch_add(r.deaf, Ordering::Relaxed);
        self.binds.fetch_add(r.binds, Ordering::Relaxed);
        self.failed.fetch_add(r.failed, Ordering::Relaxed);
        self.reverified.fetch_add(r.reverified, Ordering::Relaxed);
    }
}

/// Host settings. `clock_scale` multiplies elapsed time for the fold's
/// `now_ms` (1 in production; a live fixture sets it so the 30 s ring
/// backoff does not take minutes).
#[derive(Clone, Copy, Debug)]
pub struct HostCfg {
    pub tick: Duration,
    pub clock_scale: u64,
}

impl Default for HostCfg {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(1000),
            clock_scale: 1,
        }
    }
}

impl HostCfg {
    /// Read `FLYWHEEL_INBOX_TICK_MS` and `FLYWHEEL_INBOX_CLOCK_SCALE`.
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Some(ms) = std::env::var("FLYWHEEL_INBOX_TICK_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
        {
            c.tick = Duration::from_millis(ms);
        }
        if let Some(s) = std::env::var("FLYWHEEL_INBOX_CLOCK_SCALE")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
        {
            c.clock_scale = s;
        }
        c
    }
}

/// The running host: a dedicated thread owning the fold.
pub struct InboxHost {
    tx: mpsc::SyncSender<Input>,
    join: Option<std::thread::JoinHandle<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    pub stats: Arc<Stats>,
}

/// Process-wide signal line so the wake-socket consumer can report
/// `events_lost` without owning the host.
static SIGNAL: OnceLock<Mutex<Option<mpsc::SyncSender<Input>>>> = OnceLock::new();

/// Tell the running host (if any) that the herdr/wake stream shed frames:
/// every binding is re-verified before the next ring.
pub fn signal_invalidate(why: Invalidate) {
    if let Some(m) = SIGNAL.get()
        && let Ok(g) = m.lock()
        && let Some(tx) = g.as_ref()
    {
        let _ = tx.try_send(Input::Invalidate(why));
    }
}

impl InboxHost {
    /// Start the host thread over `ports`.
    pub fn spawn(mut ports: impl Ports, cfg: HostCfg) -> Self {
        // A tiny channel on purpose: hints are only an accelerator.
        let (tx, rx) = mpsc::sync_channel::<Input>(64);
        let stats = Arc::new(Stats::default());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (st, sp) = (stats.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("flywheel-inbox-host".into())
            .spawn(move || {
                let mut core = Core::default();
                core.restore(ports.load_budget());
                let t0 = Instant::now();
                let scale = cfg.clock_scale.max(1);
                loop {
                    let mut inputs = Vec::new();
                    match rx.recv_timeout(cfg.tick) {
                        Ok(i) => inputs.push(i),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    while let Ok(i) = rx.try_recv() {
                        inputs.push(i);
                    }
                    if sp.load(Ordering::Relaxed) {
                        break;
                    }
                    let now_ms = (t0.elapsed().as_millis() as u64).saturating_mul(scale);
                    let rep = round(&mut core, &mut ports, inputs, now_ms);
                    st.add(&rep);
                }
            })
            .expect("spawn flywheel-inbox-host");
        let sig = SIGNAL.get_or_init(|| Mutex::new(None));
        if let Ok(mut g) = sig.lock() {
            *g = Some(tx.clone());
        }
        Self {
            tx,
            join: Some(join),
            stop,
            stats,
        }
    }

    /// Look at one recipient now.
    #[allow(dead_code)]
    pub fn hint(&self, npub: &str) {
        let _ = self.tx.try_send(Input::Hint(npub.to_string()));
    }

    /// Stop and join the thread.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(sig) = SIGNAL.get()
            && let Ok(mut g) = sig.lock()
        {
            *g = None;
        }
        // Wake the recv_timeout so the stop flag is read now.
        let _ = self.tx.try_send(Input::Hint(String::new()));
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for InboxHost {
    fn drop(&mut self) {
        self.stop();
        let st = &self.stats;
        eprintln!(
            "flywheel inbox: stopped (rounds={} rings={} deaf={} binds={} reverified={} failed={})",
            st.rounds.load(Ordering::Relaxed),
            st.rings.load(Ordering::Relaxed),
            st.deaf.load(Ordering::Relaxed),
            st.binds.load(Ordering::Relaxed),
            st.reverified.load(Ordering::Relaxed),
            st.failed.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::wake::{MAX_RINGS, wake_line};

    const R: &str = "r1";

    #[derive(Default)]
    struct World {
        obs: Observation,
        reverify: Option<Bind>,
        rings: Vec<(String, u64, String)>,
        facts: Vec<(String, Value)>,
        sup: Vec<(String, u64)>,
        cap: Vec<(String, u64)>,
        reverified: u64,
        ring_fails: bool,
        budget: BTreeMap<String, Budget>,
        saves: u64,
        /// The saved budget's `rings_since_advance` at the moment of each ring.
        saved_at_ring: Vec<u32>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<World>>);

    impl Ports for Fake {
        fn load_budget(&mut self) -> BTreeMap<String, Budget> {
            self.0.lock().unwrap().budget.clone()
        }
        fn save_budget(&mut self, b: &BTreeMap<String, Budget>) -> Result<(), String> {
            let mut w = self.0.lock().unwrap();
            w.budget = b.clone();
            w.saves += 1;
            Ok(())
        }
        fn observe(&mut self) -> Observation {
            let mut w = self.0.lock().unwrap();
            let o = w.obs.clone();
            w.obs.herdr_restarted = false;
            o
        }
        fn reverify(&mut self, _npub: &str) -> Option<Bind> {
            let mut w = self.0.lock().unwrap();
            w.reverified += 1;
            w.reverify.clone()
        }
        fn ring(&mut self, n: &str, pane: &str, wm: u64, line: &str) -> Result<(), String> {
            let mut w = self.0.lock().unwrap();
            let at = w.budget.get(n).map_or(0, |b| b.rings_since_advance);
            w.saved_at_ring.push(at);
            if w.ring_fails {
                return Err("down".into());
            }
            w.rings.push((pane.into(), wm, line.into()));
            Ok(())
        }
        fn fact(&mut self, key: &str, fact: Value) -> Result<(), String> {
            let mut w = self.0.lock().unwrap();
            if !w.facts.iter().any(|(k, _)| k == key) {
                w.facts.push((key.into(), fact));
            }
            Ok(())
        }
        fn notify_supervisor(&mut self, n: &str, wm: u64, _c: DeafCause) -> Result<(), String> {
            self.0.lock().unwrap().sup.push((n.into(), wm));
            Ok(())
        }
        fn notify_captain(&mut self, n: &str, wm: u64, _c: DeafCause) -> Result<(), String> {
            self.0.lock().unwrap().cap.push((n.into(), wm));
            Ok(())
        }
    }

    fn bind(verified: bool) -> Bind {
        Bind {
            pane_id: "w1:p1".into(),
            herdr_session: "s1".into(),
            verified,
            verified_by: "report_metadata".into(),
        }
    }

    fn seen(head: u64, cursor: u64, verified: bool) -> Seen {
        Seen {
            npub: R.into(),
            pin: "TealBirch".into(),
            head,
            cursor,
            urgency: Urgency::Normal,
            pane: PaneState::Idle,
            bind: Some(bind(verified)),
        }
    }

    fn set(f: &Fake, s: Seen) {
        f.0.lock().unwrap().obs.seen = vec![s];
    }

    /// Run rounds spaced beyond the largest backoff so only the ring budget
    /// (never the clock) limits rings.
    fn run(core: &mut Core, f: &Fake, rounds: u64, t0: u64) -> u64 {
        let mut now = t0;
        for _ in 0..rounds {
            let mut p = f.clone();
            round(core, &mut p, vec![], now);
            now += 1_000_000;
        }
        now
    }

    /// "Restart the doorbell": a fresh core seeded from what the previous
    /// process persisted.
    fn restarted(f: &Fake) -> Core {
        let mut c = Core::default();
        c.restore(f.0.lock().unwrap().budget.clone());
        c
    }

    #[test]
    fn a_restart_does_not_refresh_the_ring_budget() {
        let f = Fake::default();
        set(&f, seen(4, 0, true));
        let mut core = Core::default();
        let now = run(&mut core, &f, 3, 1_000);
        assert_eq!(f.0.lock().unwrap().rings.len(), 3);
        drop(core); // the doorbell dies; the budget was persisted before each ring
        // herdr answers the re-verify of the restored (unverified) binding.
        f.0.lock().unwrap().reverify = Some(bind(true));
        let mut core2 = restarted(&f);
        run(&mut core2, &f, 10, now);
        let w = f.0.lock().unwrap();
        assert_eq!(
            w.rings.len() as u32,
            MAX_RINGS,
            "3 before + 2 after the restart = the budget, not 3 + 5"
        );
        assert_eq!(w.sup.len(), 1, "deaf escalates once");
    }

    #[test]
    fn a_restored_binding_rings_only_after_it_is_reverified() {
        let f = Fake::default();
        set(&f, seen(4, 0, true));
        let mut core = Core::default();
        let now = run(&mut core, &f, 2, 1_000);
        drop(core);
        // herdr cannot vouch for the pane yet: the restored binding stays
        // unverified and nothing is typed into it.
        f.0.lock().unwrap().reverify = None;
        let mut core2 = restarted(&f);
        run(&mut core2, &f, 3, now);
        assert_eq!(
            f.0.lock().unwrap().rings.len(),
            2,
            "no ring on a restored, unverified binding"
        );
        assert!(f.0.lock().unwrap().reverified >= 1, "it asks herdr");
        // Once herdr vouches, the remaining budget is spent and no more.
        f.0.lock().unwrap().reverify = Some(bind(true));
        core2.reverify_next_ms.clear();
        run(&mut core2, &f, 10, now + 10_000_000);
        assert_eq!(f.0.lock().unwrap().rings.len() as u32, MAX_RINGS);
    }

    #[test]
    fn a_restart_remembers_a_partially_read_cursor_so_it_is_not_mistaken_for_an_ack() {
        let f = Fake::default();
        set(&f, seen(9, 4, true)); // read up to 4 of 9
        let mut core = Core::default();
        let now = run(&mut core, &f, 2, 1_000);
        assert_eq!(f.0.lock().unwrap().rings.len(), 2);
        drop(core);
        f.0.lock().unwrap().reverify = Some(bind(true));
        let mut core2 = restarted(&f);
        run(&mut core2, &f, 10, now);
        assert_eq!(
            f.0.lock().unwrap().rings.len() as u32,
            MAX_RINGS,
            "the same cursor is not an advance: the budget carries over"
        );
    }

    #[test]
    fn a_restart_after_deaf_stays_quiet_and_never_re_escalates() {
        let f = Fake::default();
        set(&f, seen(4, 0, true));
        let mut core = Core::default();
        let now = run(&mut core, &f, 12, 1_000);
        let (rings, sup, cap) = {
            let w = f.0.lock().unwrap();
            (w.rings.len(), w.sup.len(), w.cap.len())
        };
        assert_eq!((sup, cap), (1, 1));
        let mut core2 = restarted(&f);
        run(&mut core2, &f, 12, now);
        let w = f.0.lock().unwrap();
        assert_eq!(
            w.rings.len(),
            rings,
            "no ring after a restart of a deaf recipient"
        );
        assert_eq!((w.sup.len(), w.cap.len()), (1, 1), "no second escalation");
    }

    #[test]
    fn a_restart_keeps_the_budget_only_for_the_same_pane_and_an_unadvanced_cursor() {
        // A new pane is a new incarnation: fresh budget (the fold's rule).
        let f = Fake::default();
        set(&f, seen(4, 0, true));
        let mut core = Core::default();
        let now = run(&mut core, &f, 3, 1_000);
        let mut s2 = seen(4, 0, true);
        s2.bind.as_mut().unwrap().pane_id = "w2:p9".into();
        set(&f, s2);
        let mut core2 = restarted(&f);
        run(&mut core2, &f, 10, now);
        assert_eq!(f.0.lock().unwrap().rings.len() as u32, 3 + MAX_RINGS);
        // The agent ran `toron next` while the doorbell was down: cursor moved.
        let g = Fake::default();
        set(&g, seen(4, 0, true));
        let mut c = Core::default();
        let now = run(&mut c, &g, 3, 1_000);
        set(&g, seen(4, 4, true));
        let mut c2 = restarted(&g);
        run(&mut c2, &g, 5, now);
        assert_eq!(
            g.0.lock().unwrap().rings.len(),
            3,
            "read: nothing more to ring"
        );
        assert_eq!(c2.model.recipients[R].rings_since_advance, 0);
    }

    #[test]
    fn the_budget_is_persisted_on_change_only_and_before_the_ring() {
        let f = Fake::default();
        set(&f, seen(4, 0, true));
        let mut core = Core::default();
        run(&mut core, &f, 30, 1_000);
        let w = f.0.lock().unwrap();
        assert!(
            w.saves <= u64::from(MAX_RINGS) + 1,
            "one write per budget change, not per tick: {}",
            w.saves
        );
        assert_eq!(w.budget[R].rings_since_advance, MAX_RINGS);
        assert_eq!(
            w.saved_at_ring,
            (1..=MAX_RINGS).collect::<Vec<_>>(),
            "each ring is already inside the saved budget when it goes out"
        );
        assert_eq!(w.budget[R].deaf, Some(4));
        // The ring that follows a save is already inside the saved count.
        let g = Fake::default();
        set(&g, seen(9, 0, true));
        let mut c = Core::default();
        run(&mut c, &g, 1, 1_000);
        assert_eq!(g.0.lock().unwrap().budget[R].rings_since_advance, 1);
        // Budget survives a JSON round trip (the file format).
        let j = serde_json::to_string(&g.0.lock().unwrap().budget).unwrap();
        let back: BTreeMap<String, Budget> = serde_json::from_str(&j).unwrap();
        assert_eq!(back, g.0.lock().unwrap().budget);
    }

    #[test]
    fn an_idle_recipient_leaves_no_budget_row() {
        let f = Fake::default();
        set(&f, seen(4, 4, true));
        let mut core = Core::default();
        run(&mut core, &f, 5, 1_000);
        assert!(f.0.lock().unwrap().budget.is_empty());
        assert_eq!(f.0.lock().unwrap().saves, 0);
    }

    /// 1 s ticks (the live period), not the budget-isolating spacing of `run`.
    fn ticks(core: &mut Core, f: &Fake, n: u64, t0: u64, inputs_at: Option<(u64, Input)>) -> u64 {
        let mut now = t0;
        for i in 0..n {
            let mut p = f.clone();
            let inp = match &inputs_at {
                Some((at, inp)) if *at == i => vec![inp.clone()],
                _ => vec![],
            };
            round(core, &mut p, inp, now);
            now += 1_000;
        }
        now
    }

    #[test]
    fn an_unbound_recipient_is_reverified_at_a_backed_off_pace_and_still_escalates_on_ticks() {
        let f = Fake::default();
        set(&f, seen(5, 0, false));
        let mut core = Core::default();
        ticks(&mut core, &f, 20, 1_000, None);
        let w = f.0.lock().unwrap();
        assert!(
            w.reverified <= 4,
            "20 s of unbound ticks made {} pane.list calls (was one per tick)",
            w.reverified
        );
        assert!(w.reverified >= 2, "it does keep looking");
        let deaf: Vec<_> = w
            .facts
            .iter()
            .filter(|(_, v)| v["kind"] == "deaf")
            .collect();
        assert_eq!(deaf.len(), 1, "escalation counts ticks, not reverifies");
        assert_eq!(deaf[0].1["cause"], "unbound");
        assert!(w.rings.is_empty());
    }

    #[test]
    fn a_generation_move_reverifies_at_once_through_the_backoff() {
        let f = Fake::default();
        set(&f, seen(5, 0, false));
        let mut core = Core::default();
        let now = ticks(&mut core, &f, 3, 1_000, None);
        let before = f.0.lock().unwrap().reverified;
        // inside the backoff nothing is called...
        let now = ticks(&mut core, &f, 1, now, None);
        assert_eq!(f.0.lock().unwrap().reverified, before);
        // ...until events_lost moves the generation.
        ticks(
            &mut core,
            &f,
            1,
            now,
            Some((0, Input::Invalidate(Invalidate::EventsLost))),
        );
        assert_eq!(f.0.lock().unwrap().reverified, before + 1);
    }

    #[test]
    fn a_verified_reverify_clears_the_backoff() {
        let f = Fake::default();
        set(&f, seen(5, 0, false));
        let mut core = Core::default();
        let now = ticks(&mut core, &f, 2, 1_000, None);
        assert!(core.reverify_next_ms.contains_key(R));
        f.0.lock().unwrap().reverify = Some(bind(true));
        // the next due reverify succeeds
        ticks(&mut core, &f, 5, now, None);
        assert!(!core.reverify_next_ms.contains_key(R) && !core.reverify_fails.contains_key(R));
        assert!(f.0.lock().unwrap().reverified >= 2);
    }

    #[test]
    fn the_reverify_backoff_doubles_and_is_capped() {
        assert_eq!(reverify_backoff_ms(1), 2_000);
        assert_eq!(reverify_backoff_ms(2), 4_000);
        assert_eq!(reverify_backoff_ms(3), 8_000);
        assert_eq!(reverify_backoff_ms(6), REVERIFY_BACKOFF_CAP_MS);
        assert_eq!(reverify_backoff_ms(500), REVERIFY_BACKOFF_CAP_MS);
    }

    #[test]
    fn the_production_ports_persist_the_budget_atomically() {
        let src = include_str!("inbox_ports.rs");
        assert!(src.contains("fn load_budget") && src.contains("fn save_budget"));
        let a = src.find("fn save_budget").unwrap();
        assert!(
            src[a..a + 900].contains("std::fs::rename"),
            "write-then-rename"
        );
    }

    #[test]
    fn a_message_rings_once_with_the_real_watermark_and_nothing_else() {
        let f = Fake::default();
        set(&f, seen(7, 0, true));
        let (mut core, mut p) = (Core::default(), f.clone());
        round(&mut core, &mut p, vec![], 1_000);
        // same facts, 1 s later: inside the backoff, no second ring
        round(&mut core, &mut p, vec![], 2_000);
        let w = f.0.lock().unwrap();
        assert_eq!(w.rings.len(), 1, "{:?}", w.rings);
        let (pane, wm, line) = &w.rings[0];
        assert_eq!(pane, "w1:p1", "typed by pane id");
        assert_eq!(*wm, 7, "the log head, not a counter");
        assert_eq!(line, &wake_line(7));
        assert!(line.contains("run: toron next") && !line.contains("BRAIN"));
    }

    #[test]
    fn the_watermark_follows_the_head_and_is_never_a_ring_counter() {
        let f = Fake::default();
        let (mut core, mut p) = (Core::default(), f.clone());
        set(&f, seen(3, 0, true));
        round(&mut core, &mut p, vec![], 1_000);
        // the reader runs `toron next` (cursor 3); two more arrive (head 9)
        set(&f, seen(9, 3, true));
        round(&mut core, &mut p, vec![], 2_000);
        let w = f.0.lock().unwrap();
        let wms: Vec<u64> = w.rings.iter().map(|r| r.1).collect();
        assert_eq!(wms, vec![3, 9]);
    }

    #[test]
    fn an_unanswered_message_rings_exactly_max_rings_then_goes_deaf_once() {
        // The budget is toron's (`toron_core::wake`); the live fixture and the
        // docs assume 5. A change there must be a decision here, not a drift.
        assert_eq!(MAX_RINGS, 5, "toron changed the ring budget");
        let f = Fake::default();
        set(&f, seen(4, 0, true));
        let mut core = Core::default();
        // MAX_RINGS - 1 rounds: still ringing, no deaf fact yet.
        let now = run(&mut core, &f, u64::from(MAX_RINGS) - 1, 1_000);
        {
            let w = f.0.lock().unwrap();
            assert_eq!(w.rings.len() as u32, MAX_RINGS - 1);
            assert!(w.facts.iter().all(|(_, v)| v["kind"] != "deaf"));
            assert!(w.sup.is_empty() && w.cap.is_empty());
        }
        // the MAX_RINGS-th ring goes out and nothing else yet...
        let now = run(&mut core, &f, 1, now);
        {
            let w = f.0.lock().unwrap();
            assert_eq!(w.rings.len() as u32, MAX_RINGS);
            assert!(w.sup.is_empty());
        }
        // ...the next round escalates; further rounds neither ring nor repeat.
        run(&mut core, &f, 5, now);
        let w = f.0.lock().unwrap();
        assert_eq!(w.rings.len() as u32, MAX_RINGS, "no ring past the budget");
        let deaf: Vec<_> = w
            .facts
            .iter()
            .filter(|(_, v)| v["kind"] == "deaf")
            .collect();
        assert_eq!(deaf.len(), 1, "one deaf fact per watermark");
        assert_eq!(deaf[0].1["wm"], 4);
        assert_eq!(deaf[0].1["cause"], "no_answer");
        assert_eq!(w.sup, vec![(R.to_string(), 4)], "supervisor told once");
        assert_eq!(w.cap, vec![(R.to_string(), 4)], "captain told once");
    }

    #[test]
    fn the_ack_stops_the_rings_and_clears_the_budget() {
        let f = Fake::default();
        set(&f, seen(4, 0, true));
        let mut core = Core::default();
        let now = run(&mut core, &f, 3, 1_000);
        // the reader acks: cursor reaches head
        set(&f, seen(4, 4, true));
        let now = run(&mut core, &f, 4, now);
        let before = f.0.lock().unwrap().rings.len();
        assert_eq!(before, 3);
        assert_eq!(core.model.recipients[R].rings_since_advance, 0);
        // new mail after the read rings at once with the new watermark
        set(&f, seen(6, 4, true));
        run(&mut core, &f, 1, now);
        let w = f.0.lock().unwrap();
        assert_eq!(w.rings.last().unwrap().1, 6);
        assert!(w.facts.iter().all(|(_, v)| v["kind"] != "deaf"));
    }

    #[test]
    fn a_verified_binding_writes_one_bind_fact_with_the_full_shape() {
        let f = Fake::default();
        set(&f, seen(2, 0, true));
        let mut core = Core::default();
        run(&mut core, &f, 3, 1_000);
        let w = f.0.lock().unwrap();
        let binds: Vec<_> = w
            .facts
            .iter()
            .filter(|(_, v)| v["kind"] == "bind")
            .collect();
        assert_eq!(binds.len(), 1, "{binds:?}");
        let v = &binds[0].1;
        assert_eq!(v["npub"], R);
        assert_eq!(v["pane_id"], "w1:p1");
        assert_eq!(v["herdr_session"], "s1");
        assert_eq!(v["gen"], 0);
        assert_eq!(v["verified"], true);
        assert_eq!(v["verified_by"], "report_metadata");
    }

    #[test]
    fn an_unverified_binding_is_never_rung_and_escalates_as_unbound() {
        let f = Fake::default();
        set(&f, seen(5, 0, false));
        let mut core = Core::default();
        run(&mut core, &f, u64::from(MAX_RINGS) + 2, 1_000);
        let w = f.0.lock().unwrap();
        assert!(
            w.rings.is_empty(),
            "negative control: no ring {:?}",
            w.rings
        );
        let deaf: Vec<_> = w
            .facts
            .iter()
            .filter(|(_, v)| v["kind"] == "deaf")
            .collect();
        assert_eq!(deaf.len(), 1);
        assert_eq!(deaf[0].1["cause"], "unbound");
        assert_eq!(w.sup.len(), 1);
        assert!(w.reverified > 0, "re-verification was attempted each tick");
    }

    #[test]
    fn events_lost_reverifies_the_binding_before_any_further_ring() {
        let f = Fake::default();
        set(&f, seen(2, 0, true));
        let mut core = Core::default();
        let mut p = f.clone();
        round(&mut core, &mut p, vec![], 1_000);
        assert_eq!(f.0.lock().unwrap().rings.len(), 1);
        // events_lost: the generation moves; unread mail remains
        f.0.lock().unwrap().reverify = Some(bind(true));
        let now = 2_000_000;
        round(
            &mut core,
            &mut p,
            vec![Input::Invalidate(Invalidate::EventsLost)],
            now,
        );
        {
            let w = f.0.lock().unwrap();
            assert_eq!(w.reverified, 1, "re-verified");
            assert_eq!(w.rings.len(), 1, "no ring in the invalidated round");
        }
        round(&mut core, &mut p, vec![], now + 1_000_000);
        let w = f.0.lock().unwrap();
        assert_eq!(w.rings.len(), 2, "rings again once re-verified");
        assert_eq!(core.model.events_lost, 1);
        let gens: Vec<_> = w
            .facts
            .iter()
            .filter(|(_, v)| v["kind"] == "bind")
            .map(|(_, v)| v["gen"].as_u64().unwrap())
            .collect();
        assert_eq!(gens, vec![0, 1], "a new bind fact at the new generation");
    }

    #[test]
    fn a_herdr_restart_stales_every_binding() {
        let f = Fake::default();
        set(&f, seen(2, 0, true));
        let mut core = Core::default();
        let mut p = f.clone();
        round(&mut core, &mut p, vec![], 1_000);
        f.0.lock().unwrap().obs.herdr_restarted = true;
        f.0.lock().unwrap().reverify = None; // herdr cannot confirm yet
        round(&mut core, &mut p, vec![], 2_000_000);
        round(&mut core, &mut p, vec![], 3_000_000);
        assert_eq!(f.0.lock().unwrap().rings.len(), 1, "unverified: no ring");
        assert_eq!(core.model.generation, 1);
    }

    #[test]
    fn a_failed_ring_is_counted_and_decided_again_never_assumed_delivered() {
        let f = Fake::default();
        f.0.lock().unwrap().ring_fails = true;
        set(&f, seen(2, 0, true));
        let mut core = Core::default();
        let mut p = f.clone();
        let rep = round(&mut core, &mut p, vec![], 1_000);
        assert_eq!((rep.rings, rep.failed), (0, 1));
        assert_eq!(core.model.failed_cmds, 1);
    }

    #[test]
    fn a_ring_for_a_generation_the_fold_has_invalidated_is_dropped_at_the_boundary() {
        let f = Fake::default();
        let mut core = Core::default();
        core.model.generation = 1;
        let mut p = f.clone();
        let mut rep = RoundReport::default();
        let stale = InboxCmd::Ring {
            recipient: R.into(),
            pane_id: "w1:p1".into(),
            herdr_session: "w1".into(),
            binding_gen: 0,
            wm: 2,
            line: wake_line(2),
        };
        execute(&mut core, &mut p, vec![stale], &mut rep);
        assert!(
            f.0.lock().unwrap().rings.is_empty(),
            "nothing typed for a stale gen"
        );
        assert_eq!((rep.rings, rep.failed), (0, 1));
    }

    #[test]
    fn the_host_thread_is_dedicated_and_outside_any_runtime() {
        #[derive(Clone)]
        struct Probe(Fake, Arc<Mutex<Vec<(String, bool)>>>);
        impl Ports for Probe {
            fn observe(&mut self) -> Observation {
                self.1.lock().unwrap().push((
                    std::thread::current().name().unwrap_or("").to_string(),
                    tokio::runtime::Handle::try_current().is_err(),
                ));
                self.0.observe()
            }
            fn reverify(&mut self, n: &str) -> Option<Bind> {
                self.0.reverify(n)
            }
            fn ring(&mut self, n: &str, p: &str, wm: u64, l: &str) -> Result<(), String> {
                self.0.ring(n, p, wm, l)
            }
            fn fact(&mut self, k: &str, v: Value) -> Result<(), String> {
                self.0.fact(k, v)
            }
            fn notify_supervisor(&mut self, n: &str, w: u64, c: DeafCause) -> Result<(), String> {
                self.0.notify_supervisor(n, w, c)
            }
            fn notify_captain(&mut self, n: &str, w: u64, c: DeafCause) -> Result<(), String> {
                self.0.notify_captain(n, w, c)
            }
        }
        let f = Fake::default();
        set(&f, seen(3, 0, true));
        let threads = Arc::new(Mutex::new(Vec::new()));
        // Spawn from inside a runtime: the host must still be outside one.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let host = rt.block_on(async {
            InboxHost::spawn(
                Probe(f.clone(), threads.clone()),
                HostCfg {
                    tick: Duration::from_millis(10),
                    clock_scale: 1,
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while f.0.lock().unwrap().rings.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(host.stats.rings.load(Ordering::Relaxed).max(1), 1);
        drop(host);
        let t = threads.lock().unwrap();
        assert!(!t.is_empty());
        assert!(
            t.iter()
                .all(|(n, no_rt)| n == "flywheel-inbox-host" && *no_rt),
            "{t:?}"
        );
        assert_eq!(
            f.0.lock().unwrap().rings.len(),
            1,
            "rang over the channel loop"
        );
    }

    #[test]
    fn the_wake_line_is_built_only_from_the_folds_watermark() {
        // Source fence: the host never formats a wake line itself; the only
        // constructors are toron's `wake_line` (inside the fold) and the
        // pane sink's render of the same watermark.
        let host = include_str!("inbox_host.rs");
        let body = &host[..host.find("#[cfg(test)]\nmod tests").unwrap()];
        assert!(!body.contains("new work"), "no hand-written wake text");
        assert!(
            !body.contains("wake_line("),
            "the host never builds the line"
        );
        let ports = include_str!("inbox_ports.rs");
        assert_eq!(ports.matches("pane_write::write(").count(), 1);
        // the sink's rendering must equal the fold's line, or nothing is typed
        assert!(ports.contains("!= line"), "ring compares the fold's line");
        assert!(!ports.contains("new work"));
        // (spelled in pieces: the pane-text fence scans this file too)
        let (a, b) = (
            concat!(".agent_", "prompt("),
            concat!(".pane_send_", "text("),
        );
        assert!(!ports.contains(a) && !ports.contains(b));
        let pw = include_str!("pane_write.rs");
        assert!(pw.contains("toron_core::wake::wake_line(wm.0)"));
    }
}

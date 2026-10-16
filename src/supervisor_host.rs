// governed-by: redesign-proposal §6A.12 (flywheel owns the host and interpreter), build-plan §8
//! The supervisor host (Phase 4a): interprets [`SupervisorProgram`]'s
//! commands and feeds it facts. The fold decides; this file does.
//!
//! Shape (same as the inbox host):
//! - [`Core`] holds the model and the context log. [`round`] folds a batch
//!   of messages; each message that changes the model or emits a command is
//!   written to the log *before* its commands run (write-ahead), so a kill
//!   at any point replays to the same model (P8).
//! - Every command is `Durable`: [`execute`] runs it under the effect
//!   journal ([`EffectJournal`]; production = toron's `ToronJournal`):
//!   intent recorded, claimed, settled. A key that already settled is
//!   skipped, so a replay never relaunches or escalates twice. [`recover`]
//!   re-runs what the log says was asked and the journal says never
//!   finished.
//! - [`Ports`] is the effect boundary (herdr, mail, `br`, the captain
//!   page). The fold never touches it.
//!
//! The context log is the supervisor's fact log: one JSON line per accepted
//! message, `{"seq","msg","in_off"}`. Operators and crews add facts through
//! `flywheel supervisor fact|steer` into `<ctx>.in.jsonl`; the host is the
//! only writer of `<ctx>.log.jsonl` and records how far it has read the
//! input file, so a restart neither loses nor repeats an input.
//!
//! Gap (named, not hidden): steer@1/escalate@1 contracts and the
//! `sup_epoch` fence at append are specified for toron-core (§6A.12) and do
//! not exist there at 80356f5. Until they do, the fence is the fold's
//! (`Control` below the accepted epoch is rejected before anything is sent)
//! and a steer travels as a `notify@1` envelope whose text names the op.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toron_core::kernel::{Cmd as _, Durability, Program};

use crate::supervisor_program::{
    CrewRow, PASS, Policy, Restart, SteerOp, SupCmd, SupModel, SupMsg, SupervisorProgram, Tier,
    ChildSpec, crew_view,
};

/// Journal answer for one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Begin {
    /// Run it; settle with this generation.
    Run(u64),
    /// Already finished: skip.
    Skip,
    /// The journal could not be reached: do not run blind.
    Unavailable(String),
}

/// The effect journal a command runs under.
pub trait EffectJournal {
    /// Record the intent (idempotent on `key`) and claim it.
    fn begin(&mut self, key: &str) -> Begin;
    /// Settle the claim.
    fn finish(&mut self, key: &str, generation: u64, ok: bool);
}

/// The effect boundary. Each method is one interpreted command.
pub trait Ports {
    /// Facts this round: registry spawns, cursors, deaf facts, herdr edges.
    fn observe(&mut self, model: &SupModel, now_ms: u64) -> Vec<SupMsg>;
    /// Start incarnation `generation` of `child`.
    fn relaunch(&mut self, child: &str, generation: u64) -> Result<(), String>;
    /// Stop `child`'s pane.
    fn exit(&mut self, child: &str, generation: u64) -> Result<(), String>;
    /// Append the steer envelope; urgent ones interrupt and ring. Returns
    /// the log offset.
    fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String>;
    /// Tell the captain and record the fact.
    fn escalate(&mut self, n: u64, reason: &str, evidence: &str) -> Result<(), String>;
    /// Park a poisoned child.
    fn park(&mut self, child: &str, incarnations: u32) -> Result<(), String>;
    /// Page the captain.
    fn page(&mut self, kind: &str, pin: &str, line: &str) -> Result<(), String>;
    /// Record a rejected input.
    fn reject(&mut self, n: u64, cause: &str, child: &str) -> Result<(), String>;
}

/// What `append_steer` needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteerSend {
    /// Steer seq.
    pub seq: u64,
    /// Target child.
    pub child: String,
    /// Op.
    pub op: SteerOp,
    /// Incarnation.
    pub generation: u64,
    /// Epoch.
    pub sup_epoch: u64,
    /// Interrupt and ring as well.
    pub urgent: bool,
}

// ---- the context log ------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Entry {
    seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    in_off: Option<u64>,
    msg: SupMsg,
}

/// Paths of one context's files.
#[derive(Clone, Debug)]
pub struct CtxPaths {
    /// The authoritative log (host-only writer).
    pub log: PathBuf,
    /// Operator/crew inputs (CLI appends).
    pub inbox: PathBuf,
}

impl CtxPaths {
    /// Files for `project` under `dir`.
    pub fn new(dir: &Path, project: &str) -> Self {
        let key = crate::state::state_key(project);
        Self {
            log: dir.join(format!("{key}.log.jsonl")),
            inbox: dir.join(format!("{key}.in.jsonl")),
        }
    }

    /// The default state directory.
    pub fn default_dir() -> PathBuf {
        crate::config::state_dir().join("supervisor")
    }
}

/// Read the log: its messages in order, the next seq and the input offset.
pub fn read_log(p: &CtxPaths) -> Result<(Vec<SupMsg>, u64, u64)> {
    let mut msgs = Vec::new();
    let (mut seq, mut in_off) = (0u64, 0u64);
    let Ok(s) = std::fs::read_to_string(&p.log) else {
        return Ok((msgs, 0, 0));
    };
    for (i, l) in s.lines().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        let e: Entry =
            serde_json::from_str(l).with_context(|| format!("{}:{}: bad log line", p.log.display(), i + 1))?;
        seq = e.seq + 1;
        if let Some(o) = e.in_off {
            in_off = o;
        }
        msgs.push(e.msg);
    }
    Ok((msgs, seq, in_off))
}

fn append_log(p: &CtxPaths, e: &Entry) -> Result<()> {
    if let Some(d) = p.log.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p.log)?;
    writeln!(f, "{}", serde_json::to_string(e)?)?;
    f.sync_data()?;
    Ok(())
}

/// Read complete input lines from `from`; returns them and the new offset
/// (a half-written last line is left for the next read).
pub fn read_inputs(p: &CtxPaths, from: u64) -> (Vec<SupMsg>, u64) {
    let Ok(mut f) = std::fs::File::open(&p.inbox) else {
        return (Vec::new(), from);
    };
    let mut buf = String::new();
    if std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(from)).is_err()
        || f.read_to_string(&mut buf).is_err()
    {
        return (Vec::new(), from);
    }
    let mut out = Vec::new();
    let mut used = 0usize;
    for l in buf.split_inclusive('\n') {
        if !l.ends_with('\n') {
            break;
        }
        used += l.len();
        match serde_json::from_str::<SupMsg>(l.trim()) {
            Ok(m) => out.push(m),
            Err(e) => eprintln!("flywheel supervisor: skipping a bad input line: {e}"),
        }
    }
    (out, from + used as u64)
}

/// CLI side: add one input fact.
pub fn append_input(p: &CtxPaths, msg: &SupMsg) -> Result<()> {
    if let Some(d) = p.inbox.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p.inbox)?;
    // One write per line: O_APPEND keeps concurrent writers' lines whole.
    f.write_all(format!("{}\n", serde_json::to_string(msg)?).as_bytes())?;
    Ok(())
}

// ---- the core: model + log ------------------------------------------------

/// Model, log and read position.
pub struct Core {
    /// The fold's model.
    pub model: SupModel,
    paths: CtxPaths,
    seq: u64,
    in_off: u64,
    pending_in_off: Option<u64>,
}

/// What a round did (tests and the host's status line read it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoundReport {
    /// Messages folded.
    pub folded: u64,
    /// Messages written to the log.
    pub logged: u64,
    /// Commands run to success.
    pub ran: u64,
    /// Commands skipped (journal says settled).
    pub skipped: u64,
    /// Commands that failed.
    pub failed: u64,
}

impl Core {
    /// Open a context: replay the log. A fresh context starts with `Hello`;
    /// every open then claims the next `sup_epoch` (the lease fact), so a
    /// restarted supervisor outranks whatever it replaced.
    pub fn open(paths: CtxPaths, me: &str, tier: Tier, policy: Option<Policy>) -> Result<Self> {
        let (msgs, seq, in_off) = read_log(&paths)?;
        let (model, _) = crate::supervisor_program::replay(&msgs);
        let mut core = Self {
            model,
            paths,
            seq,
            in_off,
            pending_in_off: None,
        };
        if msgs.is_empty() {
            core.append_fold(SupMsg::Hello {
                me: me.into(),
                tier,
                policy,
            })?;
        }
        Ok(core)
    }

    /// Claim the next epoch for `holder`.
    pub fn claim(&mut self, holder: &str) -> Result<u64> {
        if self.model.me != holder {
            self.append_fold(SupMsg::Rebind { me: holder.into() })?;
        }
        let e = self.model.accepted_epoch + 1;
        self.append_fold(SupMsg::Claim {
            holder: holder.into(),
            sup_epoch: e,
        })?;
        Ok(e)
    }

    fn append_fold(&mut self, m: SupMsg) -> Result<Vec<SupCmd>> {
        let u = SupervisorProgram::update(&self.model, &m);
        if u.model != self.model || !u.commands.is_empty() {
            append_log(
                &self.paths,
                &Entry {
                    seq: self.seq,
                    in_off: self.pending_in_off.take(),
                    msg: m,
                },
            )?;
            self.seq += 1;
        }
        self.model = u.model;
        Ok(u.commands)
    }

    /// The paths this core reads and writes.
    pub fn paths(&self) -> &CtxPaths {
        &self.paths
    }
}

// ---- executing commands ---------------------------------------------------

/// Ring an urgent steer: an `Interrupt`, then the `URGENT steer (wm N)` wake
/// line, but only if this is the first wake for `(npub, wm)` across both
/// hosts. Returns whether it rang. At-most-once: a re-run of the durable
/// command after a crash does not ring again (the steer's ack window is the
/// backstop for an unanswered steer).
pub(crate) fn ring_urgent_steer(
    ledger: &crate::ring_ledger::RingLedger,
    typer: &dyn crate::pane_write::PaneTyper,
    npub: &str,
    pane: &str,
    offset: u64,
) -> anyhow::Result<bool> {
    use crate::pane_write::{PaneWrite, Urgency, Wm, write};
    if !ledger.steer_may_ring(npub, offset) {
        return Ok(false);
    }
    write(typer, pane, &PaneWrite::Interrupt)?;
    write(
        typer,
        pane,
        &PaneWrite::WakeLine {
            wm: Wm(offset),
            urgency: Urgency::Urgent,
        },
    )?;
    Ok(true)
}

fn key_of(c: &SupCmd) -> String {
    match c.durability() {
        Durability::Durable { key } => key,
        Durability::Ephemeral => String::new(),
    }
}

/// Run one command under the journal. Returns the messages its outcome
/// feeds back into the fold.
pub fn execute(
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
    cmd: &SupCmd,
    rep: &mut RoundReport,
) -> Vec<SupMsg> {
    let key = key_of(cmd);
    let generation = match j.begin(&key) {
        Begin::Skip => {
            rep.skipped += 1;
            return Vec::new();
        }
        Begin::Unavailable(why) => {
            rep.failed += 1;
            return vec![SupMsg::CmdFailed {
                key,
                why: format!("journal unavailable: {why}"),
            }];
        }
        Begin::Run(g) => g,
    };
    let res: Result<Vec<SupMsg>, String> = match cmd {
        SupCmd::Relaunch { child, generation } => ports.relaunch(child, *generation).map(|_| {
            vec![SupMsg::Relaunched {
                child: child.clone(),
                generation: *generation,
            }]
        }),
        SupCmd::Exit { child, generation } => ports.exit(child, *generation).map(|_| vec![]),
        SupCmd::AppendSteer {
            seq,
            child,
            op,
            generation,
            sup_epoch,
            urgent,
        } => ports
            .append_steer(&SteerSend {
                seq: *seq,
                child: child.clone(),
                op: *op,
                generation: *generation,
                sup_epoch: *sup_epoch,
                urgent: *urgent,
            })
            .map(|offset| vec![SupMsg::SteerAppended { seq: *seq, offset }]),
        SupCmd::Escalate {
            n,
            reason,
            evidence,
        } => ports.escalate(*n, reason, evidence).map(|_| vec![]),
        SupCmd::Park { child, incarnations } => ports.park(child, *incarnations).map(|_| vec![]),
        SupCmd::Page { kind, pin, line, .. } => ports.page(kind, pin, line).map(|_| vec![]),
        SupCmd::Reject { n, cause, child } => ports.reject(*n, cause.tag(), child).map(|_| vec![]),
    };
    j.finish(&key, generation, res.is_ok());
    match res {
        Ok(v) => {
            rep.ran += 1;
            v
        }
        Err(why) => {
            rep.failed += 1;
            vec![SupMsg::CmdFailed { key, why }]
        }
    }
}

/// Fold `msgs` (and the messages their commands feed back) to quiescence.
pub fn feed(
    core: &mut Core,
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
    msgs: Vec<SupMsg>,
    rep: &mut RoundReport,
) -> Result<()> {
    let mut q: VecDeque<SupMsg> = msgs.into();
    while let Some(m) = q.pop_front() {
        rep.folded += 1;
        let before = core.seq;
        let cmds = core.append_fold(m)?;
        rep.logged += core.seq - before;
        for c in &cmds {
            q.extend(execute(j, ports, c, rep));
        }
    }
    Ok(())
}

/// One host round: read inputs, observe, tick, fold, execute.
pub fn round(
    core: &mut Core,
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
    extra: Vec<SupMsg>,
    now_ms: u64,
) -> Result<RoundReport> {
    let mut rep = RoundReport::default();
    let (inputs, off) = read_inputs(&core.paths, core.in_off);
    if off != core.in_off {
        core.in_off = off;
        core.pending_in_off = Some(off);
    }
    let mut msgs = extra;
    msgs.extend(inputs);
    msgs.extend(ports.observe(&core.model, now_ms));
    msgs.push(SupMsg::Tick { now_ms });
    feed(core, j, ports, msgs, &mut rep)?;
    // Inputs that changed nothing still advance the read position.
    if let Some(off) = core.pending_in_off.take() {
        core.in_off = off;
        // Persisted with the next logged message; a restart re-reads them
        // and the fold ignores them again (deterministic no-ops).
        core.pending_in_off = Some(off);
    }
    Ok(rep)
}

/// After a restart: run every command the log asked for that the journal
/// never settled. Commands are idempotent by key, so re-running one whose
/// outcome was lost is safe.
pub fn recover(
    core: &mut Core,
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
) -> Result<RoundReport> {
    let (msgs, _, _) = read_log(&core.paths)?;
    let (_, cmds) = crate::supervisor_program::replay(&msgs);
    let mut rep = RoundReport::default();
    let mut follow = Vec::new();
    for c in &cmds {
        follow.extend(execute(j, ports, c, &mut rep));
    }
    feed(core, j, ports, follow, &mut rep)?;
    Ok(rep)
}

// ---- production journal ---------------------------------------------------

/// toron's `ToronJournal` as the effect journal.
pub struct ToronEffectJournal {
    j: toron_core::journal::ToronJournal,
    owner: String,
    lease_ms: i64,
}

impl ToronEffectJournal {
    /// Wrap `j`; claims are owned by `owner`.
    pub fn new(j: toron_core::journal::ToronJournal, owner: &str) -> Self {
        Self {
            j,
            owner: owner.into(),
            lease_ms: 60_000,
        }
    }

    /// Open `scope` on the shared comms index database.
    pub fn open(scope: &str, owner: &str) -> Result<Self> {
        let mb = crate::mail_shim::comms_mailbox()?;
        let pool = mb
            .index_pool()
            .cloned()
            .ok_or_else(|| anyhow!("supervisor journal needs the index pool"))?;
        let rt = crate::mail_shim::runtime()?;
        let db = toron_core::journal::JournalDb::new(pool, scope, 4096);
        Ok(Self::new(
            toron_core::journal::ToronJournal::spawn(db, rt.handle()),
            owner,
        ))
    }
}

impl EffectJournal for ToronEffectJournal {
    fn begin(&mut self, key: &str) -> Begin {
        use toron_core::kernel::{EffectState, JournalStore};
        let row = match self.j.try_record_intent(key, 3) {
            Ok(r) => r,
            Err(e) => return Begin::Unavailable(e.to_string()),
        };
        if row.state.is_terminal() {
            return Begin::Skip;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let claimed = match self.j.try_claim(key, &self.owner, now + self.lease_ms) {
            Ok(Some(r)) => r,
            Ok(None) => return Begin::Unavailable("claim refused".into()),
            Err(e) => return Begin::Unavailable(e.to_string()),
        };
        if claimed.state == EffectState::Prepared {
            let _ = self.j.settle(key, claimed.generation, EffectState::InFlight);
        }
        Begin::Run(claimed.generation)
    }

    fn finish(&mut self, key: &str, generation: u64, ok: bool) {
        use toron_core::kernel::{EffectState, JournalStore};
        let to = if ok {
            EffectState::Settled
        } else {
            EffectState::Failed
        };
        let _ = self.j.settle(key, generation, to);
    }
}

/// In-memory journal (tests; also what a dry run uses).
#[derive(Default)]
pub struct MemJournal {
    /// key -> (settled?, ok, runs)
    pub rows: BTreeMap<String, (bool, bool, u64)>,
}

impl EffectJournal for MemJournal {
    fn begin(&mut self, key: &str) -> Begin {
        let r = self.rows.entry(key.into()).or_insert((false, false, 0));
        if r.0 {
            return Begin::Skip;
        }
        r.2 += 1;
        Begin::Run(r.2)
    }

    fn finish(&mut self, key: &str, generation: u64, ok: bool) {
        if let Some(r) = self.rows.get_mut(key)
            && r.2 == generation
        {
            r.0 = true;
            r.1 = ok;
        }
    }
}

// ---- production ports -----------------------------------------------------

/// herdr + mail + the captain page, as the supervisor's effect boundary.
pub struct LivePorts {
    cfg: crate::config::Config,
    project: String,
    inbox: crate::inbox_ports::FlywheelPorts,
    herdr_up: Option<bool>,
    facts_seen: BTreeSet<String>,
    deaf_off: u64,
    relaunch_bin: PathBuf,
}

impl LivePorts {
    /// Ports for `project`.
    pub fn new(cfg: &crate::config::Config, project: &str, workspace: &str) -> Self {
        let relaunch_bin = std::env::var_os("FLYWHEEL_SUPERVISOR_RELAUNCH_BIN")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("flywheel"));
        Self {
            cfg: cfg.clone(),
            project: project.into(),
            inbox: crate::inbox_ports::FlywheelPorts::new(cfg, project, workspace),
            herdr_up: None,
            facts_seen: BTreeSet::new(),
            deaf_off: 0,
            relaunch_bin,
        }
    }

    fn facts_path(&self) -> PathBuf {
        crate::config::state_dir()
            .join("facts")
            .join(format!("{}-supervisor.jsonl", crate::state::state_key(&self.project)))
    }

    fn inbox_facts_path(&self) -> PathBuf {
        crate::config::state_dir()
            .join("facts")
            .join(format!("{}-inbox.jsonl", crate::state::state_key(&self.project)))
    }

    fn fact(&mut self, key: &str, mut v: Value) -> Result<(), String> {
        if !self.facts_seen.insert(key.to_string()) {
            return Ok(());
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("key".into(), json!(key));
            o.insert("project".into(), json!(self.project));
            o.insert("ts".into(), json!(chrono::Utc::now().to_rfc3339()));
        }
        let path = self.facts_path();
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(f, "{v}").map_err(|e| e.to_string())
    }

    fn pane_of(&self, pin: &str) -> Option<String> {
        crate::state::load_spawn_registry(&self.project)
            .into_iter()
            .find(|e| e.mail_identity.as_deref() == Some(pin))
            .map(|e| e.pane_id)
    }

    fn client(&self) -> Option<crate::herdr_shim::Client> {
        crate::herdr_shim::Client::connect(Some(&self.cfg.socket_path())).ok()
    }

    fn page_captain(&self, line: &str) {
        crate::page_captain_dead_stop(&self.cfg, &self.project, line);
    }
}

impl Ports for LivePorts {
    fn observe(&mut self, model: &SupModel, now_ms: u64) -> Vec<SupMsg> {
        let mut out = Vec::new();
        // herdr edges.
        let up = self.client().is_some();
        if self.herdr_up != Some(up) {
            if self.herdr_up.is_some() || !up {
                out.push(if up {
                    SupMsg::HerdrUp { now_ms }
                } else {
                    SupMsg::HerdrDown { now_ms }
                });
            }
            self.herdr_up = Some(up);
        }
        // Crews the registry knows that the model does not.
        for e in crate::state::load_spawn_registry(&self.project) {
            let Some(pin) = e.mail_identity.clone() else {
                continue;
            };
            if model.children.contains_key(&pin) {
                continue;
            }
            let npub = self.inbox.npub_of(&pin);
            out.push(SupMsg::Spawn {
                spec: ChildSpec {
                    id: pin,
                    npub,
                    restart: Restart::Transient,
                },
                now_ms,
            });
        }
        // Cursors, for children with a steer in flight (the ack).
        for s in model.steers.values() {
            if s.state != crate::supervisor_program::SteerState::Pending || s.offset.is_none() {
                continue;
            }
            let Some(npub) = model.children.get(&s.child).and_then(|c| c.spec.npub.clone()) else {
                continue;
            };
            if let Some(store) = self.inbox.store().cloned() {
                let (_, cursor) = crate::mail_shim::block_on(async move {
                    crate::inbox_ports::head_and_cursor(&store, &npub).await
                });
                out.push(SupMsg::Cursor {
                    child: s.child.clone(),
                    cursor,
                });
            }
        }
        // The inbox host's deaf facts, by the npub they name.
        if let Ok(mut f) = std::fs::File::open(self.inbox_facts_path())
            && std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(self.deaf_off)).is_ok()
        {
            let mut buf = String::new();
            if f.read_to_string(&mut buf).is_ok() {
                let mut used = 0usize;
                for l in buf.split_inclusive('\n') {
                    if !l.ends_with('\n') {
                        break;
                    }
                    used += l.len();
                    let Ok(v) = serde_json::from_str::<Value>(l.trim()) else {
                        continue;
                    };
                    if v["kind"] != "deaf" {
                        continue;
                    }
                    let (Some(npub), Some(key)) = (v["npub"].as_str(), v["key"].as_str()) else {
                        continue;
                    };
                    if let Some((id, _)) = model
                        .children
                        .iter()
                        .find(|(_, c)| c.spec.npub.as_deref() == Some(npub))
                    {
                        out.push(SupMsg::Deaf {
                            child: id.clone(),
                            key: key.to_string(),
                            now_ms,
                        });
                    }
                }
                self.deaf_off += used as u64;
            }
        }
        out
    }

    fn relaunch(&mut self, child: &str, generation: u64) -> Result<(), String> {
        if child == PASS {
            // The next interval is the relaunch of the pass child.
            return Ok(());
        }
        let st = std::process::Command::new(&self.relaunch_bin)
            .args(["crew", "control", &self.project, child, "relaunch"])
            .env("FLYWHEEL_SUPERVISOR_GENERATION", generation.to_string())
            .status()
            .map_err(|e| format!("spawn relaunch: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("relaunch {child}: {st}"))
        }
    }

    fn exit(&mut self, child: &str, _generation: u64) -> Result<(), String> {
        if child == PASS {
            return Ok(());
        }
        let pane = self.pane_of(child).ok_or_else(|| format!("no pane for {child}"))?;
        let c = self.client().ok_or("herdr unavailable")?;
        crate::pane_write::write(&crate::pane_write::HerdrPanes(&c), &pane, &crate::pane_write::PaneWrite::Exit)
            .map_err(|e| format!("{e:#}"))
    }

    fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String> {
        let text = format!(
            "steer {} on {} (seq {}, generation {}, sup_epoch {}) — run: toron next",
            s.op.tag(),
            s.child,
            s.seq,
            s.generation,
            s.sup_epoch
        );
        let offset =
            self.inbox
                .append_notify(&s.child, text, &format!("steer:{}", s.seq), s.generation)?;
        if s.urgent
            && let (Some(pane), Some(c), Some(npub)) =
                (self.pane_of(&s.child), self.client(), self.inbox.npub_of(&s.child))
        {
            ring_urgent_steer(
                &self.inbox.ring_ledger(),
                &crate::pane_write::HerdrPanes(&c),
                &npub,
                &pane,
                offset,
            )
            .map_err(|e| format!("{e:#}"))?;
        }
        Ok(offset)
    }

    fn escalate(&mut self, n: u64, reason: &str, evidence: &str) -> Result<(), String> {
        let line = format!("loop-mode supervise: ESCALATE {reason} in {} — {evidence}", self.project);
        eprintln!("{line}");
        self.fact(
            &format!("escalate:{n}:{reason}"),
            json!({"kind": "escalate", "reason": reason, "evidence": evidence}),
        )?;
        self.page_captain(&line);
        Ok(())
    }

    fn park(&mut self, child: &str, incarnations: u32) -> Result<(), String> {
        let line = format!(
            "loop-mode supervise: PARKED {child} in {} after {incarnations} abnormal incarnations (ceiling; not retried)",
            self.project
        );
        eprintln!("{line}");
        self.fact(
            &format!("park:{child}"),
            json!({"kind": "park", "child": child, "incarnations": incarnations}),
        )?;
        self.page_captain(&line);
        Ok(())
    }

    fn page(&mut self, kind: &str, pin: &str, line: &str) -> Result<(), String> {
        eprintln!("{line} — paging the captain once");
        self.fact(
            &format!("page:{kind}:{pin}:{}", chrono::Utc::now().timestamp()),
            json!({"kind": "page", "page": kind, "pin": pin}),
        )?;
        self.page_captain(line);
        Ok(())
    }

    fn reject(&mut self, n: u64, cause: &str, child: &str) -> Result<(), String> {
        eprintln!("flywheel supervisor: rejected ({cause}) child={child}");
        self.fact(
            &format!("reject:{n}:{cause}"),
            json!({"kind": "reject", "cause": cause, "child": child}),
        )
    }
}

// ---- the host loop (`loop-mode supervise` under the comms flag) -----------

/// Settings from the environment (fixtures shorten these; production uses
/// the defaults).
#[derive(Clone, Debug)]
pub struct HostCfg {
    /// Loop period.
    pub tick_ms: u64,
    /// Steer ack window / child lease overrides.
    pub ack_ms: Option<u64>,
    /// Child lease override.
    pub lease_ms: Option<u64>,
    /// Do not run loop passes (crews only).
    pub passes: bool,
}

impl HostCfg {
    /// Read `FLYWHEEL_SUPERVISOR_{TICK_MS,ACK_MS,LEASE_MS,PASSES}`.
    pub fn from_env() -> Self {
        let num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        Self {
            tick_ms: num("FLYWHEEL_SUPERVISOR_TICK_MS").unwrap_or(1_000).max(10),
            ack_ms: num("FLYWHEEL_SUPERVISOR_ACK_MS"),
            lease_ms: num("FLYWHEEL_SUPERVISOR_LEASE_MS"),
            passes: std::env::var("FLYWHEEL_SUPERVISOR_PASSES").map_or(true, |v| v != "off"),
        }
    }

    fn policy(&self) -> Option<Policy> {
        if self.ack_ms.is_none() && self.lease_ms.is_none() {
            return None;
        }
        let mut p = Policy::for_tier(Tier::Lead);
        if let Some(a) = self.ack_ms {
            p.ack_ms = a;
        }
        if let Some(l) = self.lease_ms {
            p.lease_ms = l;
        }
        Some(p)
    }
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// `loop-mode supervise` under the comms flag: the same cadence, rehosted
/// on [`SupervisorProgram`]. Returns when the supervisor failed (intensity
/// exceeded: escalated, subtree cancelled) or was superseded.
pub fn run_supervise(
    cfg: &crate::config::Config,
    project: &str,
    work: bool,
    interval_secs: u64,
    to: &str,
    stall_minutes: u64,
) -> Result<()> {
    let hc = HostCfg::from_env();
    let paths = CtxPaths::new(&CtxPaths::default_dir(), project);
    let me = format!("supervise-{project}-{}", std::process::id());
    // One live host per context (the log has one writer).
    let lease_ws = format!("supervise-{}", crate::state::state_key(project));
    let sd = crate::config::state_dir();
    let now_s = || chrono::Utc::now().timestamp().max(0) as u64;
    let mut lease = match doorbell_rt::lease::acquire_lease(&sd, &lease_ws, now_s()) {
        Ok(l) => l,
        Err(doorbell_rt::lease::LeaseDenied::Held(o)) => {
            bail!("another supervisor host (pid {}) holds {project}; not starting a second", o.pid)
        }
        Err(_) => bail!("cannot write the supervisor lease"),
    };
    let result = host_loop(cfg, project, work, interval_secs, to, stall_minutes, &hc, &paths, &me, &lease_ws, &mut lease);
    doorbell_rt::lease::release_lease(&sd, &lease_ws, &lease);
    result
}

#[allow(clippy::too_many_arguments)]
fn host_loop(
    cfg: &crate::config::Config,
    project: &str,
    work: bool,
    interval_secs: u64,
    to: &str,
    stall_minutes: u64,
    hc: &HostCfg,
    paths: &CtxPaths,
    me: &str,
    lease_ws: &str,
    lease: &mut doorbell_rt::lease::OwnerLease,
) -> Result<()> {
    let mut core = Core::open(paths.clone(), me, Tier::Lead, hc.policy())?;
    let epoch = core.claim(me)?;
    let mut journal = ToronEffectJournal::open(&format!("supervise:{}", crate::state::state_key(project)), me)?;
    let mut ports = LivePorts::new(cfg, project, project);
    eprintln!(
        "flywheel supervisor: {project} hosted on SupervisorProgram (sup_epoch {epoch}, {} children replayed, MaxR {}/{}s) — comms flag on",
        core.model.children.len(),
        core.model.policy.max_r,
        core.model.policy.max_t_ms / 1000
    );
    let rec = recover(&mut core, &mut journal, &mut ports)?;
    if rec.ran + rec.skipped > 0 {
        eprintln!("flywheel supervisor: recovery ran {} command(s), {} already settled", rec.ran, rec.skipped);
    }
    let exe = std::env::current_exe().context("resolve the flywheel executable")?;
    let project_dir = crate::project_dir(cfg, project);
    let stall_secs = stall_minutes.max(1) * 60;
    let mut extra = Vec::new();
    if hc.passes && !core.model.children.contains_key(PASS) {
        extra.push(SupMsg::Spawn {
            spec: ChildSpec {
                id: PASS.into(),
                npub: None,
                restart: Restart::Permanent,
            },
            now_ms: now_ms(),
        });
    }
    let mut pass: Option<std::process::Child> = None;
    let mut last_pass_end = std::time::Instant::now() - std::time::Duration::from_secs(interval_secs);
    loop {
        let now = now_ms();
        if let Some(c) = pass.as_mut()
            && let Some(st) = c.try_wait().context("poll the pass child")?
        {
            pass = None;
            last_pass_end = std::time::Instant::now();
            extra.push(SupMsg::PassExit {
                ok: st.success(),
                now_ms: now,
            });
            // The dead-stop and wedge probes ride the end of a pass, as before.
            extra.extend(probe_msgs(cfg, project, &project_dir, stall_secs, &core.model, now));
        }
        let pass_live = core
            .model
            .children
            .get(PASS)
            .is_some_and(|c| c.state.live_for_host());
        if hc.passes
            && pass.is_none()
            && pass_live
            && last_pass_end.elapsed() >= std::time::Duration::from_secs(interval_secs)
        {
            let argv = crate::durable::supervise_pass_argv(project, work, to);
            pass = Some(
                std::process::Command::new(&exe)
                    .args(&argv)
                    .spawn()
                    .with_context(|| format!("spawn `{}`", argv.join(" ")))?,
            );
        }
        let rep = round(&mut core, &mut journal, &mut ports, std::mem::take(&mut extra), now)?;
        if rep.failed > 0 {
            eprintln!("flywheel supervisor: {} command(s) failed this round", rep.failed);
        }
        doorbell_rt::lease::refresh_lease(&crate::config::state_dir(), lease_ws, lease, chrono::Utc::now().timestamp().max(0) as u64);
        match core.model.status {
            crate::supervisor_program::Status::Failed => {
                if let Some(mut c) = pass.take() {
                    let _ = c.kill();
                }
                bail!(
                    "loop-mode supervise stopping: restart intensity exceeded in {project} ({} restarts inside {}s, max {}); the subtree is cancelled and the captain was paged — fix the cause, then re-run supervise",
                    core.model.restarts.len(),
                    core.model.policy.max_t_ms / 1000,
                    core.model.policy.max_r
                );
            }
            crate::supervisor_program::Status::Superseded => {
                if let Some(mut c) = pass.take() {
                    let _ = c.kill();
                }
                bail!(
                    "loop-mode supervise stopping: superseded by {} at sup_epoch {}",
                    core.model.leader,
                    core.model.accepted_epoch
                );
            }
            crate::supervisor_program::Status::Running => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(hc.tick_ms));
    }
}

fn probe_msgs(
    cfg: &crate::config::Config,
    project: &str,
    project_dir: &Path,
    stall_secs: u64,
    model: &SupModel,
    now_ms: u64,
) -> Vec<SupMsg> {
    let now = now_ms / 1000;
    let mut out = Vec::new();
    let probe = crate::cmd::captain::probe_stall(cfg, project, project_dir, now, stall_secs);
    let action = crate::durable::dead_stop_action(&crate::durable::DeadStopInputs {
        work_remaining: probe.work_remaining,
        fleet_knowable: probe.fleet_knowable,
        any_pane_working: probe.any_pane_working,
        last_commit_age_secs: probe.last_commit_age_secs,
        stall_secs,
        already_escalated: model.stalled,
    });
    match action {
        crate::durable::DeadStopAction::Escalate(kind) => {
            let age = probe.last_commit_age_secs.unwrap_or(0);
            let line = match kind {
                crate::durable::DeadStopKind::SilentPanes => format!(
                    "loop-mode supervise: DEAD STOP in {project} — no pane working and no commit for {age}s (>{stall_secs}s) with work remaining"
                ),
                crate::durable::DeadStopKind::StuckWorking => format!(
                    "loop-mode supervise: STUCK PANES in {project} — pane(s) report working but nothing moved for {age}s (>{stall_secs}s)"
                ),
            };
            out.push(SupMsg::Stalled { line });
        }
        crate::durable::DeadStopAction::Clear if model.stalled => out.push(SupMsg::WorkMoved),
        _ => {}
    }
    if probe.last_commit_age_secs.is_some_and(|a| a < stall_secs) {
        out.push(SupMsg::WorkMoved);
    }
    let paged: std::collections::HashSet<String> = model.paged.iter().cloned().collect();
    for a in crate::probe_pane_wedges(cfg, project, project_dir, now, stall_secs, &paged) {
        if let crate::durable::PaneWedgeAction::Page { pin, pane_id, error } = a {
            out.push(SupMsg::Wedged {
                line: format!(
                    "loop-mode supervise: PANE WEDGE in {project} — {pin} ({pane_id}) is alive but parked on: {error} (nothing committed for {}s)",
                    probe.last_commit_age_secs.unwrap_or(0)
                ),
                pin,
            });
        }
    }
    out
}

// ---- the operator/crew CLI (`flywheel supervisor ...`) --------------------

/// `flywheel supervisor` subcommands.
#[derive(clap::Subcommand, Debug)]
pub enum SupervisorCmd {
    /// A crew (or a script) reports a fact for the supervisor's log.
    Fact {
        project: String,
        /// Child (pin).
        #[arg(long)]
        child: String,
        /// progress | done | exit
        #[arg(long)]
        kind: String,
        /// The incarnation the report is for (required for progress/done).
        #[arg(long, default_value_t = 0)]
        generation: u64,
    },
    /// Steer a child: redirect | pause | resume | cancel.
    Steer {
        project: String,
        #[arg(long)]
        child: String,
        #[arg(long)]
        op: String,
        /// The sup_epoch the sender holds (default: the log's accepted one).
        #[arg(long)]
        sup_epoch: Option<u64>,
    },
    /// Print the CrewView from the log (read-only; replays it).
    View {
        project: String,
        #[arg(long)]
        json: bool,
    },
}

/// Parse a steer op word.
pub fn parse_op(s: &str) -> Result<SteerOp> {
    Ok(match s {
        "redirect" => SteerOp::Redirect,
        "pause" => SteerOp::Pause,
        "resume" => SteerOp::Resume,
        "cancel" => SteerOp::Cancel,
        other => bail!("unknown steer op {other:?} (redirect|pause|resume|cancel)"),
    })
}

/// Run a `flywheel supervisor` subcommand.
pub fn run_cli(cmd: &SupervisorCmd) -> Result<()> {
    match cmd {
        SupervisorCmd::Fact {
            project,
            child,
            kind,
            generation,
        } => {
            let p = CtxPaths::new(&CtxPaths::default_dir(), project);
            let now_ms = now_ms();
            let msg = match kind.as_str() {
                "progress" => SupMsg::Progress { child: child.clone(), generation: *generation, now_ms },
                "done" => SupMsg::Done { child: child.clone(), generation: *generation, now_ms },
                "exit" => SupMsg::Exit { child: child.clone(), now_ms },
                other => bail!("unknown fact kind {other:?} (progress|done|exit)"),
            };
            append_input(&p, &msg)?;
            println!("queued {kind} for {child}");
            Ok(())
        }
        SupervisorCmd::Steer {
            project,
            child,
            op,
            sup_epoch,
        } => {
            let p = CtxPaths::new(&CtxPaths::default_dir(), project);
            let op = parse_op(op)?;
            let epoch = match sup_epoch {
                Some(e) => *e,
                None => {
                    let (msgs, _, _) = read_log(&p)?;
                    crate::supervisor_program::replay(&msgs).0.accepted_epoch
                }
            };
            append_input(
                &p,
                &SupMsg::Control {
                    op,
                    child: child.clone(),
                    sup_epoch: epoch,
                    now_ms: now_ms(),
                },
            )?;
            println!("queued steer {} for {child} under sup_epoch {epoch}", op.tag());
            Ok(())
        }
        SupervisorCmd::View { project, json } => {
            let p = CtxPaths::new(&CtxPaths::default_dir(), project);
            let (msgs, _, _) = read_log(&p)?;
            let (m, _) = crate::supervisor_program::replay(&msgs);
            let rows = crew_view(&m);
            if *json {
                println!("{}", serde_json::to_string_pretty(&json!({
                    "status": m.status, "sup_epoch": m.accepted_epoch, "leader": m.leader,
                    "restarts_in_window": m.restarts.len(), "rows": rows,
                }))?);
            } else {
                print_view(&m, &rows);
            }
            Ok(())
        }
    }
}

fn print_view(m: &SupModel, rows: &[CrewRow]) {
    println!(
        "supervisor {:?} sup_epoch {} leader {} restarts {}/{}",
        m.status, m.accepted_epoch, m.leader, m.restarts.len(), m.policy.max_r
    );
    for r in rows {
        let steer = r
            .pending_steer
            .map(|(s, o)| format!(" steer#{s}:{}", o.tag()))
            .unwrap_or_default();
        println!("  {:<20} {:?} gen {} abnormal {}{steer}", r.child, r.state, r.generation, r.abnormal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor_program::{ChildState, SteerState, Status};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct World {
        obs: Vec<SupMsg>,
        relaunched: Vec<(String, u64)>,
        exited: Vec<(String, u64)>,
        steers: Vec<SteerSend>,
        escalated: Vec<(u64, String)>,
        parked: Vec<String>,
        pages: Vec<(String, String)>,
        rejects: Vec<(u64, String)>,
        next_offset: u64,
        fail_relaunch: bool,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<World>>);

    impl Ports for Fake {
        fn observe(&mut self, _m: &SupModel, _now: u64) -> Vec<SupMsg> {
            std::mem::take(&mut self.0.lock().unwrap().obs)
        }
        fn relaunch(&mut self, c: &str, g: u64) -> Result<(), String> {
            let mut w = self.0.lock().unwrap();
            if w.fail_relaunch {
                return Err("herdr said no".into());
            }
            w.relaunched.push((c.into(), g));
            Ok(())
        }
        fn exit(&mut self, c: &str, g: u64) -> Result<(), String> {
            self.0.lock().unwrap().exited.push((c.into(), g));
            Ok(())
        }
        fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String> {
            let mut w = self.0.lock().unwrap();
            w.next_offset += 10;
            w.steers.push(s.clone());
            Ok(w.next_offset)
        }
        fn escalate(&mut self, n: u64, r: &str, _e: &str) -> Result<(), String> {
            self.0.lock().unwrap().escalated.push((n, r.into()));
            Ok(())
        }
        fn park(&mut self, c: &str, _n: u32) -> Result<(), String> {
            self.0.lock().unwrap().parked.push(c.into());
            Ok(())
        }
        fn page(&mut self, k: &str, p: &str, _l: &str) -> Result<(), String> {
            self.0.lock().unwrap().pages.push((k.into(), p.into()));
            Ok(())
        }
        fn reject(&mut self, n: u64, c: &str, _child: &str) -> Result<(), String> {
            self.0.lock().unwrap().rejects.push((n, c.into()));
            Ok(())
        }
    }

    fn open(dir: &Path) -> Core {
        let paths = CtxPaths::new(dir, "proj");
        let mut c = Core::open(paths, "lead-1", Tier::Lead, None).unwrap();
        c.claim("lead-1").unwrap();
        c
    }

    fn spawn(id: &str) -> SupMsg {
        SupMsg::Spawn {
            spec: ChildSpec {
                id: id.into(),
                npub: Some(format!("npub-{id}")),
                restart: Restart::Transient,
            },
            now_ms: 0,
        }
    }

    fn go(c: &mut Core, j: &mut MemJournal, f: &Fake, extra: Vec<SupMsg>, now: u64) -> RoundReport {
        let mut p = f.clone();
        round(c, j, &mut p, extra, now).unwrap()
    }

    #[test]
    fn a_crashed_crew_is_relaunched_once_at_the_next_generation() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let rep = go(&mut c, &mut j, &f, vec![SupMsg::Exit { child: "a".into(), now_ms: 2_000 }], 2_000);
        assert_eq!(f.0.lock().unwrap().relaunched, vec![("a".to_string(), 2)]);
        assert_eq!(rep.ran, 1);
        assert_eq!(c.model.children["a"].state, ChildState::Running, "Relaunched fed back");
        // A tick later changes nothing and relaunches nothing.
        go(&mut c, &mut j, &f, vec![], 3_000);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1);
    }

    #[test]
    fn a_kill_at_any_point_replays_to_the_same_model_and_view() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        let epoch = c.model.accepted_epoch;
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Control {
                op: SteerOp::Redirect,
                child: "a".into(),
                sup_epoch: epoch,
                now_ms: 2_000,
            }],
            2_000,
        );
        go(&mut c, &mut j, &f, vec![SupMsg::Exit { child: "b".into(), now_ms: 3_000 }], 3_000);
        let want = c.model.clone();
        drop(c);
        // "kill -9": reopen from the log alone.
        let paths = CtxPaths::new(d.path(), "proj");
        let (msgs, _, _) = read_log(&paths).unwrap();
        let (replayed, _) = crate::supervisor_program::replay(&msgs);
        assert_eq!(replayed, want);
        assert_eq!(crew_view(&replayed), crew_view(&want));
        assert!(replayed.steers.values().any(|s| s.state == SteerState::Pending));
        assert_eq!(replayed.children.len(), 2, "same child set");
    }

    #[test]
    fn reopening_claims_the_next_epoch_and_keeps_the_children() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        assert_eq!(c.model.accepted_epoch, 1);
        drop(c);
        let c2 = open(d.path());
        assert_eq!(c2.model.accepted_epoch, 2);
        assert!(c2.model.children.contains_key("a"));
        assert_eq!(c2.model.status, Status::Running);
    }

    #[test]
    fn a_different_host_process_over_the_same_log_runs_it_and_replay_agrees() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        drop(c);
        let mut c2 = Core::open(CtxPaths::new(d.path(), "proj"), "lead-2", Tier::Lead, None).unwrap();
        c2.claim("lead-2").unwrap();
        assert_eq!(c2.model.status, Status::Running, "a restart is not its own supersession");
        assert_eq!(c2.model.leader, "lead-2");
        let (msgs, _, _) = read_log(&CtxPaths::new(d.path(), "proj")).unwrap();
        let (replayed, _) = crate::supervisor_program::replay(&msgs);
        assert_eq!(replayed, c2.model);
    }

    #[test]
    fn recovery_runs_what_the_log_asked_and_the_journal_never_settled_and_skips_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        go(&mut c, &mut j, &f, vec![SupMsg::Exit { child: "a".into(), now_ms: 2_000 }], 2_000);
        go(&mut c, &mut j, &f, vec![SupMsg::Exit { child: "b".into(), now_ms: 2_100 }], 2_100);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 2);
        // Crash model: the second relaunch's journal row never settled.
        j.rows.get_mut("relaunch:b:2").unwrap().0 = false;
        drop(c);
        let f2 = Fake::default();
        let mut c2 = open(d.path());
        let mut p2 = f2.clone();
        let rep = recover(&mut c2, &mut j, &mut p2).unwrap();
        assert_eq!(f2.0.lock().unwrap().relaunched, vec![("b".to_string(), 2)], "only the unsettled one");
        assert!(rep.skipped >= 1);
    }

    #[test]
    fn a_failed_relaunch_is_fed_back_and_spends_the_budget() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        f.0.lock().unwrap().fail_relaunch = true;
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let rep = go(&mut c, &mut j, &f, vec![SupMsg::Exit { child: "a".into(), now_ms: 2_000 }], 2_000);
        assert!(rep.failed >= 1);
        assert!(c.model.failed_cmds >= 1);
        assert!(
            matches!(c.model.children["a"].state, ChildState::Parked)
                || c.model.status == Status::Failed
                || c.model.restarts.len() >= 2,
            "each failed relaunch is another incarnation, bounded by poison/intensity"
        );
        assert_eq!(c.model.children["a"].generation >= 2, true);
    }

    #[test]
    fn a_steer_is_appended_urgent_then_acked_by_the_cursor_and_the_gate_opens() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let e = c.model.accepted_epoch;
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Control { op: SteerOp::Cancel, child: "a".into(), sup_epoch: e, now_ms: 2_000 }],
            2_000,
        );
        let s = f.0.lock().unwrap().steers.clone();
        assert_eq!(s.len(), 1);
        assert!(s[0].urgent && s[0].op == SteerOp::Cancel && s[0].generation == 1);
        assert_eq!(c.model.steers[&1].offset, Some(10), "the offset came back as a fact");
        // While pending, the child's done is refused (P13).
        go(&mut c, &mut j, &f, vec![SupMsg::Done { child: "a".into(), generation: 1, now_ms: 2_500 }], 2_500);
        assert_eq!(f.0.lock().unwrap().rejects.last().unwrap().1, "steer_pending");
        // The cursor reaches the steer: acked.
        f.0.lock().unwrap().obs.push(SupMsg::Cursor { child: "a".into(), cursor: 10 });
        go(&mut c, &mut j, &f, vec![], 3_000);
        assert_eq!(c.model.steers[&1].state, SteerState::Acked);
    }

    #[test]
    fn a_stale_epoch_cancel_from_the_input_file_is_rejected_and_nothing_is_sent() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        c.claim("lead-1").unwrap(); // epoch 2
        append_input(
            c.paths(),
            &SupMsg::Control { op: SteerOp::Cancel, child: "a".into(), sup_epoch: 1, now_ms: 2_000 },
        )
        .unwrap();
        go(&mut c, &mut j, &f, vec![], 2_000);
        let w = f.0.lock().unwrap();
        assert!(w.steers.is_empty(), "no envelope, no interrupt");
        assert_eq!(w.rejects.last().unwrap().1, "stale_epoch");
        assert!(c.model.steers.is_empty());
    }

    #[test]
    fn inputs_are_read_once_across_a_restart_and_a_half_line_waits() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let p = c.paths().clone();
        append_input(&p, &SupMsg::Exit { child: "a".into(), now_ms: 2_000 }).unwrap();
        // A torn write: no newline yet.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p.inbox)
            .unwrap()
            .write_all(b"{\"kind\":\"exit\"")
            .unwrap();
        go(&mut c, &mut j, &f, vec![], 2_000);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1);
        drop(c);
        let mut c2 = open(d.path());
        go(&mut c2, &mut j, &f, vec![], 3_000);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1, "not re-read");
    }

    #[test]
    fn quiet_ticks_do_not_grow_the_log() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let before = std::fs::read_to_string(&c.paths().log).unwrap().lines().count();
        for t in 2..20u64 {
            go(&mut c, &mut j, &f, vec![], t * 100);
        }
        let after = std::fs::read_to_string(&c.paths().log).unwrap().lines().count();
        assert_eq!(before, after);
    }

    #[test]
    fn the_fourth_failure_pages_once_and_stops_the_supervisor() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        let ex = |id: &str, t| SupMsg::Exit { child: id.into(), now_ms: t };
        for (i, id) in ["a", "b", "a", "b"].iter().enumerate() {
            go(&mut c, &mut j, &f, vec![ex(id, 2_000 + i as u64)], 2_000 + i as u64);
        }
        let w = f.0.lock().unwrap();
        assert_eq!(w.relaunched.len(), 3);
        assert_eq!(w.escalated.len(), 1, "one escalation");
        assert_eq!(w.escalated[0].1, "restart_intensity");
        drop(w);
        assert_eq!(c.model.status, Status::Failed);
        assert_eq!(f.0.lock().unwrap().exited.len(), 2, "the subtree was cancelled");
    }

    /// A pane that records every write, shared by both hosts in a test.
    #[derive(Default)]
    struct RecPane(std::sync::Mutex<Vec<String>>);

    impl crate::pane_write::PaneTyper for RecPane {
        fn prompt(&self, target: &str, line: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("{target} line {line}"));
            Ok(())
        }
        fn keys(&self, target: &str, keys: &[&str]) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("{target} keys {}", keys.join("+")));
            Ok(())
        }
        fn close(&self, target: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("{target} close"));
            Ok(())
        }
    }

    fn urgent_lines(p: &RecPane) -> usize {
        p.0.lock().unwrap().iter().filter(|l| l.contains("URGENT steer")).count()
    }

    fn normal_lines(p: &RecPane) -> usize {
        p.0.lock().unwrap().iter().filter(|l| l.contains("line") && !l.contains("URGENT")).count()
    }

    fn inbox_ring(l: &crate::ring_ledger::RingLedger, p: &RecPane, wm: u64) {
        let line = crate::pane_write::wake_line(crate::pane_write::Wm(wm), crate::pane_write::Urgency::Normal);
        crate::inbox_ports::ring_wake(l, p, "npub1a", "w1:a", wm, &line).unwrap();
    }

    #[test]
    fn one_urgent_steer_is_one_ring_counting_both_hosts_supervisor_first() {
        let d = tempfile::tempdir().unwrap();
        let l = crate::ring_ledger::RingLedger::new(d.path().join("rings"));
        let p = RecPane::default();
        assert!(ring_urgent_steer(&l, &p, "npub1a", "w1:a", 7).unwrap());
        // The inbox host sees the same mail at wm 7 and wants to ring it.
        inbox_ring(&l, &p, 7);
        assert_eq!(urgent_lines(&p), 1);
        assert_eq!(normal_lines(&p), 0, "no second ring for the same mail");
        assert_eq!(p.0.lock().unwrap().iter().filter(|w| w.contains("keys")).count(), 1, "one interrupt");
        // Still unread: the inbox host's budgeted re-ring is a real ring.
        inbox_ring(&l, &p, 7);
        assert_eq!(normal_lines(&p), 1, "re-rings stay the doorbell's");
    }

    #[test]
    fn one_urgent_steer_is_one_ring_counting_both_hosts_inbox_first() {
        let d = tempfile::tempdir().unwrap();
        let l = crate::ring_ledger::RingLedger::new(d.path().join("rings"));
        let p = RecPane::default();
        inbox_ring(&l, &p, 7);
        assert!(!ring_urgent_steer(&l, &p, "npub1a", "w1:a", 7).unwrap(), "already woken");
        assert_eq!(urgent_lines(&p), 0);
        assert_eq!(normal_lines(&p), 1);
        assert!(p.0.lock().unwrap().iter().all(|w| !w.contains("keys")), "no interrupt either");
    }

    #[test]
    fn a_rerun_steer_command_does_not_ring_twice_and_a_later_mail_rings() {
        let d = tempfile::tempdir().unwrap();
        let l = crate::ring_ledger::RingLedger::new(d.path().join("rings"));
        let p = RecPane::default();
        assert!(ring_urgent_steer(&l, &p, "npub1a", "w1:a", 7).unwrap());
        assert!(!ring_urgent_steer(&l, &p, "npub1a", "w1:a", 7).unwrap());
        assert!(ring_urgent_steer(&l, &p, "npub1a", "w1:a", 8).unwrap(), "a second steer is its own wake");
        assert_eq!(urgent_lines(&p), 2);
    }

    #[test]
    fn a_stale_claims_settle_does_not_close_the_key() {
        let mut j = MemJournal::default();
        let g1 = match j.begin("relaunch:a:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        let g2 = match j.begin("relaunch:a:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        assert!(g2 > g1);
        j.finish("relaunch:a:2", g1, true);
        assert!(matches!(j.begin("relaunch:a:2"), Begin::Run(_)), "the older claim settled nothing");
        j.finish("relaunch:a:2", g2 + 1, true);
        assert_eq!(j.begin("relaunch:a:2"), Begin::Skip, "the current claim settles it");
    }

    // ---- the real journal -------------------------------------------------

    fn toron_journal() -> (ToronEffectJournal, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        let pool = rt.block_on(async {
            let pool = toron_core::index::schema::connect(&db.to_string_lossy()).await.unwrap();
            toron_core::index::schema::migrate(&pool).await.unwrap();
            pool
        });
        let jd = toron_core::journal::JournalDb::new(pool, "supervise:test", 64);
        (
            ToronEffectJournal::new(toron_core::journal::ToronJournal::spawn(jd, rt.handle()), "me"),
            rt,
        )
    }

    #[test]
    fn toron_journal_runs_a_key_once_and_reruns_one_that_never_settled() {
        let (mut j, _rt) = toron_journal();
        let g = match j.begin("relaunch:a:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        j.finish("relaunch:a:2", g, true);
        assert_eq!(j.begin("relaunch:a:2"), Begin::Skip, "settled: never again");
        // Claimed, effect lost, never settled: runs again under a newer claim.
        let g1 = match j.begin("relaunch:b:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        let g2 = match j.begin("relaunch:b:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        assert!(g2 > g1, "a newer claim fences the older");
        j.finish("relaunch:b:2", g1, true); // the stale claim settles nothing
        assert!(matches!(j.begin("relaunch:b:2"), Begin::Run(_)));
        // A failed settle is terminal too.
        let g3 = match j.begin("park:c") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        j.finish("park:c", g3, false);
        assert_eq!(j.begin("park:c"), Begin::Skip);
    }

    #[test]
    fn the_host_runs_end_to_end_on_the_toron_journal() {
        let d = tempfile::tempdir().unwrap();
        let (mut j, _rt) = toron_journal();
        let f = Fake::default();
        let mut c = open(d.path());
        let mut p = f.clone();
        round(&mut c, &mut j, &mut p, vec![spawn("a")], 1_000).unwrap();
        round(&mut c, &mut j, &mut p, vec![SupMsg::Exit { child: "a".into(), now_ms: 2_000 }], 2_000).unwrap();
        assert_eq!(f.0.lock().unwrap().relaunched, vec![("a".to_string(), 2)]);
        // Same command again (a replay): the journal says settled.
        let mut rep = RoundReport::default();
        let again = execute(&mut j, &mut p, &SupCmd::Relaunch { child: "a".into(), generation: 2 }, &mut rep);
        assert!(again.is_empty() && rep.skipped == 1);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1);
    }

    // ---- flag gate and shape ----------------------------------------------

    #[test]
    fn the_flag_off_supervise_path_is_unchanged_and_the_flag_on_path_is_this_host() {
        let src = include_str!("main.rs");
        let a = src.find("fn cmd_loop_supervise(").unwrap();
        let body = &src[a..a + 2_600];
        let gate = body.find("if comms_send::enabled(cfg) {").expect("flag gate");
        let legacy = body.find("supervise_failure_count(").expect("legacy path kept");
        assert!(gate < legacy, "the gate precedes the unchanged legacy loop");
        assert!(body[gate..legacy].contains("supervisor_host::run_supervise("));
        assert!(src.contains("SUPERVISE_MAX_CONSECUTIVE_FAILURES"), "legacy rule still defined for flag-off");
    }

    #[test]
    fn the_host_loop_runs_on_a_plain_thread_and_the_fold_never_shells_out() {
        let me = include_str!("supervisor_host.rs");
        let prog = include_str!("supervisor_program.rs");
        // The fold is pure: no process, fs, clock, or runtime in it.
        let pure = &prog[..prog.find("#[cfg(test)]").unwrap()];
        for banned in ["std::process", "std::fs", "std::time", "SystemTime", "Instant", "tokio", "block_on", "HashMap"] {
            assert!(!pure.contains(banned), "supervisor_program must stay pure: {banned}");
        }
        // The host never types into a pane except through the PaneWrite sink.
        let host = &me[..me.find("#[cfg(test)]").unwrap()];
        for call in crate::pane_write::PANE_TEXT_CALLS {
            assert!(!host.contains(call), "pane text call {call} outside the sink");
        }
    }
}

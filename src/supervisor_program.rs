// governed-by: redesign-proposal §6A (supervisor topology), build-plan §8 (Phase 4a)
//! `SupervisorProgram`: the pure fold behind `loop-mode supervise` (Phase 4a).
//!
//! `(Model, Msg) -> (Model, Vec<Cmd>)`, no clock, no store, no `HashMap`, no
//! shell-out. Time arrives as `now_ms` on a message. The host
//! ([`crate::supervisor_host`]) interprets the commands; the fold only
//! decides.
//!
//! What it owns (redesign §6A.2, §6A.11):
//! - **Children** as data: `child{spec, generation, lease}`; `restart` is
//!   `permanent | transient | temporary`; a transient child ends normally
//!   only through `Done`. The strategy is data too (`one_for_one` in 4a).
//! - **Restart intensity (P11)**: at most `max_r` restarts per `max_t`
//!   (leads 3 / 30 min, captain 2 / 60 min). The restart that would exceed
//!   it is not made: the subtree is cancelled, the supervisor is `Failed`,
//!   one `Escalate` is emitted and nothing restarts after it. This replaces
//!   "3 consecutive failed passes, then stop".
//! - **Generation fencing (P3/P10)**: a restart is a new incarnation at
//!   `generation + 1`; `Progress`/`Done` carrying an older generation are rejected.
//! - **Leases**: a child holds a lease renewed by `Progress`; past its
//!   deadline it ends abnormally and restarts at `generation + 1`.
//! - **`sup_epoch` fence (P9)**: `Control` carrying an epoch below the
//!   accepted one is rejected; a `Claim` with a higher epoch supersedes
//!   this supervisor, which then acts on nothing.
//! - **Steer gate (P13)**: while a steer on a child is unacked, no
//!   `Progress` or `Done` from it is accepted. A steer is acked by the
//!   child's cursor reaching the steer's log offset (it ran `toron next`),
//!   never by a side channel. One not acked by `ack_by` is resolved by
//!   restart (`applied_by_restart`) or, for `cancel`, by exit (P10).
//! - **Poison**: a child ending abnormally 3 incarnations in a row is
//!   parked as a ceiling.
//! - **herdr down**: log-only. No restarts; one escalation; the children
//!   needing one restart (through the intensity check) once herdr is back.
//! - the loop pass child (`pass`), the wedge page set and the dead-stop
//!   flag that `cmd_loop_supervise` kept in locals.
//!
//! Replay (P8): the model is `fold(msgs)`. Every field is ordered and
//! serializable, so the same log gives the same model and the same
//! `CrewView`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use toron_core::kernel::{Boot, Cmd as KCmd, Durability, Failure, Program, Update};

/// Crew-level leads: restarts per window (§6A.2).
pub const LEAD_MAX_R: u32 = 3;
/// Crew-level leads: window.
pub const LEAD_MAX_T_MS: u64 = 30 * 60_000;
/// Captain: restarts per window.
pub const CAPTAIN_MAX_R: u32 = 2;
/// Captain: window.
pub const CAPTAIN_MAX_T_MS: u64 = 60 * 60_000;
/// Abnormal incarnations of one child before it is parked.
pub const POISON_INCARNATIONS: u32 = 3;
/// Panes one supervisor holds (ADR-0011 D2).
pub const CREW_CAP: usize = 5;
/// Default time a child has to ack a steer.
pub const DEFAULT_ACK_MS: u64 = 120_000;
/// Default child lease.
pub const DEFAULT_LEASE_MS: u64 = 600_000;
/// The loop pass child's id.
pub const PASS: &str = "pass";

/// Which tier this supervisor is (picks the intensity budget).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A plane lead (`loop-mode supervise`): 3 per 30 min.
    Lead,
    /// The captain (root): 2 per 60 min.
    Captain,
}

/// Restart rule of a child (§6A.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    /// Always restarted.
    Permanent,
    /// Restarted only after an abnormal end.
    Transient,
    /// Never restarted.
    Temporary,
}

/// What a failure restarts. Data, so 4b adds `RestForOne`/`OneForAll`
/// without touching the intensity code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Only the failed child.
    OneForOne,
}

/// The supervisor's budgets and windows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Tier these were taken from.
    pub tier: Tier,
    /// What a failure restarts.
    pub strategy: Strategy,
    /// Restarts allowed per `max_t_ms`.
    pub max_r: u32,
    /// Intensity window.
    pub max_t_ms: u64,
    /// Abnormal incarnations before a child is parked.
    pub poison: u32,
    /// Panes held at once.
    pub crew_cap: usize,
    /// Steer ack window.
    pub ack_ms: u64,
    /// Child lease.
    pub lease_ms: u64,
}

impl Policy {
    /// The settled defaults for a tier.
    pub fn for_tier(tier: Tier) -> Self {
        let (max_r, max_t_ms) = match tier {
            Tier::Lead => (LEAD_MAX_R, LEAD_MAX_T_MS),
            Tier::Captain => (CAPTAIN_MAX_R, CAPTAIN_MAX_T_MS),
        };
        Self {
            tier,
            strategy: Strategy::OneForOne,
            max_r,
            max_t_ms,
            poison: POISON_INCARNATIONS,
            crew_cap: CREW_CAP,
            ack_ms: DEFAULT_ACK_MS,
            lease_ms: DEFAULT_LEASE_MS,
        }
    }
}

/// A child's spec (`child{spec}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSpec {
    /// Pin (or [`PASS`]).
    pub id: String,
    /// The child's npub, when it has an inbox.
    pub npub: Option<String>,
    /// Restart rule.
    pub restart: Restart,
}

/// Where a child is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildState {
    /// Live incarnation.
    Running,
    /// Relaunch asked, not yet confirmed.
    Restarting,
    /// Needs a restart that herdr's absence deferred.
    NeedsRestart,
    /// Ended normally (`Done`). Final.
    Done,
    /// Cancelled by a steer or a subtree cancel. Final.
    Cancelled,
    /// Poison ceiling reached. Final until an operator acts.
    Parked,
    /// Ended and not restarted (temporary). Final.
    Gone,
}

impl ChildState {
    fn live(self) -> bool {
        matches!(self, Self::Running | Self::Restarting | Self::NeedsRestart)
    }
}

/// One child.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Child {
    /// Spec.
    pub spec: ChildSpec,
    /// State.
    pub state: ChildState,
    /// Incarnation = lease epoch = task generation.
    pub generation: u64,
    /// Abnormal incarnations since the last `Done`.
    pub abnormal: u32,
    /// Lease deadline.
    pub lease_deadline_ms: u64,
    /// Last accepted progress.
    pub last_progress_ms: u64,
    /// Start order (for `rest_for_one` in 4b).
    pub order: u64,
    /// Lease expiry suspended (`pause`).
    pub paused: bool,
}

/// What a steer asks (steer@1 ops).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerOp {
    /// Change course.
    Redirect,
    /// Stop working until `resume`.
    Pause,
    /// Continue.
    Resume,
    /// Stop for good.
    Cancel,
}

impl SteerOp {
    /// Tag for keys and facts.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Redirect => "redirect",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        }
    }
}

/// A steer's resolution (P10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerState {
    /// Sent, unacked: gates the child (P13).
    Pending,
    /// The child's cursor passed it.
    Acked,
    /// A newer steer on the same child replaced it.
    Superseded,
    /// The child was restarted (or killed) instead.
    AppliedByRestart,
    /// Neither: the restart budget was spent.
    Escalated,
}

/// A steer record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steer {
    /// Sequence (per supervisor log).
    pub seq: u64,
    /// Target child.
    pub child: String,
    /// Op.
    pub op: SteerOp,
    /// Gen the steer was issued at.
    pub generation: u64,
    /// Epoch it was issued under.
    pub sup_epoch: u64,
    /// Deadline for the ack.
    pub ack_by_ms: u64,
    /// Log offset of the appended envelope, once known.
    pub offset: Option<u64>,
    /// State.
    pub state: SteerState,
}

/// Why a message was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCause {
    /// Epoch below the accepted one (P9).
    StaleEpoch,
    /// Epoch above any claim.
    UnclaimedEpoch,
    /// Generation behind the child's (P3).
    StaleGen,
    /// A steer is unacked (P13).
    SteerPending,
    /// No such live child.
    UnknownChild,
    /// Crew budget spent.
    CapExceeded,
    /// This supervisor is not running.
    NotRunning,
}

impl RejectCause {
    /// Tag for keys and facts.
    pub fn tag(self) -> &'static str {
        match self {
            Self::StaleEpoch => "stale_epoch",
            Self::UnclaimedEpoch => "unclaimed_epoch",
            Self::StaleGen => "stale_gen",
            Self::SteerPending => "steer_pending",
            Self::UnknownChild => "unknown_child",
            Self::CapExceeded => "cap_exceeded",
            Self::NotRunning => "not_running",
        }
    }
}

/// The supervisor's own state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Deciding.
    Running,
    /// Intensity exceeded; escalated; no further restarts.
    Failed,
    /// A newer `sup_epoch` holds the lease.
    Superseded,
}

/// The model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupModel {
    /// This supervisor's holder id.
    pub me: String,
    /// Budgets.
    pub policy: Policy,
    /// Status.
    pub status: Status,
    /// Highest `sup_epoch` accepted (the lease's).
    pub accepted_epoch: u64,
    /// Holder of that epoch.
    pub leader: String,
    /// Children by id.
    pub children: BTreeMap<String, Child>,
    /// Steers by seq.
    pub steers: BTreeMap<u64, Steer>,
    /// Restart times inside the window.
    pub restarts: Vec<u64>,
    /// Next child order.
    pub next_order: u64,
    /// Next steer seq.
    pub steer_seq: u64,
    /// Escalations emitted.
    pub escalations: u64,
    /// Messages rejected (keys the reject facts).
    pub rejects: u64,
    /// Pages emitted (keys the page commands).
    pub pages: u64,
    /// herdr reachable.
    pub herdr_up: bool,
    /// herdr-down already escalated.
    pub herdr_escalated: bool,
    /// Pins paged for a wedge since work last moved.
    pub paged: BTreeSet<String>,
    /// Dead-stop already paged.
    pub stalled: bool,
    /// Commands that failed.
    pub failed_cmds: u64,
    /// Latest time seen.
    pub now_ms: u64,
}

impl SupModel {
    /// A fresh model for `me` at `tier`.
    pub fn new(me: &str, tier: Tier) -> Self {
        Self {
            me: me.into(),
            policy: Policy::for_tier(tier),
            status: Status::Running,
            accepted_epoch: 0,
            leader: String::new(),
            children: BTreeMap::new(),
            steers: BTreeMap::new(),
            restarts: Vec::new(),
            next_order: 0,
            steer_seq: 0,
            escalations: 0,
            rejects: 0,
            pages: 0,
            herdr_up: true,
            herdr_escalated: false,
            paged: BTreeSet::new(),
            stalled: false,
            failed_cmds: 0,
            now_ms: 0,
        }
    }

    fn pending_steer(&self, child: &str) -> Option<&Steer> {
        self.steers
            .values()
            .find(|s| s.child == child && s.state == SteerState::Pending)
    }

    fn live_crews(&self) -> usize {
        self.children
            .values()
            .filter(|c| c.spec.id != PASS && c.state.live())
            .count()
    }
}

impl Default for SupModel {
    fn default() -> Self {
        Self::new("", Tier::Lead)
    }
}

/// Inputs. Every one is a fact or a clock reading; the host persists them
/// in order and the model is their fold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SupMsg {
    /// First message: who this supervisor is and which tier.
    Hello {
        /// Holder id.
        me: String,
        /// Tier.
        tier: Tier,
        /// Policy overrides (tests and fixtures); `None` = settled defaults.
        policy: Option<Policy>,
    },
    /// `lead{holder, sup_epoch}`: a supervisor takes the lease.
    Claim {
        /// Holder.
        holder: String,
        /// Epoch.
        sup_epoch: u64,
    },
    /// `child{spec}`: adopt a child at generation 1.
    Spawn {
        /// Spec.
        spec: ChildSpec,
        /// Time.
        now_ms: u64,
    },
    /// The child made progress at `generation`.
    Progress {
        /// Child.
        child: String,
        /// Gen it reports.
        generation: u64,
        /// Time.
        now_ms: u64,
    },
    /// The child finished at `generation`.
    Done {
        /// Child.
        child: String,
        /// Gen it reports.
        generation: u64,
        /// Time.
        now_ms: u64,
    },
    /// The harness exited without `Done`.
    Exit {
        /// Child.
        child: String,
        /// Time.
        now_ms: u64,
    },
    /// The inbox host's `deaf` fact for the child.
    Deaf {
        /// Child.
        child: String,
        /// Time.
        now_ms: u64,
    },
    /// The child's committed cursor (it ran `toron next`).
    Cursor {
        /// Child.
        child: String,
        /// Cursor.
        cursor: u64,
    },
    /// A steer request under `sup_epoch`.
    Control {
        /// Op.
        op: SteerOp,
        /// Target.
        child: String,
        /// Epoch the sender holds.
        sup_epoch: u64,
        /// Time.
        now_ms: u64,
    },
    /// The steer's envelope was appended at `offset`.
    SteerAppended {
        /// Steer seq.
        seq: u64,
        /// Log offset.
        offset: u64,
    },
    /// The relaunch landed.
    Relaunched {
        /// Child.
        child: String,
        /// Gen.
        generation: u64,
    },
    /// herdr is unreachable.
    HerdrDown {
        /// Time.
        now_ms: u64,
    },
    /// herdr is back.
    HerdrUp {
        /// Time.
        now_ms: u64,
    },
    /// A loop pass ended.
    PassExit {
        /// Exit success.
        ok: bool,
        /// Time.
        now_ms: u64,
    },
    /// A pane is alive but parked on a fatal line.
    Wedged {
        /// Pin.
        pin: String,
        /// The page text.
        line: String,
    },
    /// No pane working and no commit inside the window.
    Stalled {
        /// The page text.
        line: String,
    },
    /// A commit landed: wedge and dead-stop probes re-arm.
    WorkMoved,
    /// Clock.
    Tick {
        /// Time.
        now_ms: u64,
    },
    /// A command failed.
    CmdFailed {
        /// Its durable key (empty if not durable).
        key: String,
        /// Why.
        why: String,
    },
}

/// Intents. The Durable ones carry a key, so the host's journal runs each
/// once and a replay skips what settled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SupCmd {
    /// Start a new incarnation at `generation`.
    Relaunch {
        /// Child.
        child: String,
        /// Gen.
        generation: u64,
    },
    /// Stop the child's pane (cancel, subtree cancel).
    Exit {
        /// Child.
        child: String,
        /// Gen.
        generation: u64,
    },
    /// Append the steer envelope; urgent ones also interrupt and ring.
    AppendSteer {
        /// Steer seq.
        seq: u64,
        /// Target.
        child: String,
        /// Op.
        op: SteerOp,
        /// Gen.
        generation: u64,
        /// Epoch.
        sup_epoch: u64,
        /// Interrupt + urgent wake line as well.
        urgent: bool,
    },
    /// Tell the parent (the captain) and record the fact.
    Escalate {
        /// Count (keys it).
        n: u64,
        /// Reason tag.
        reason: String,
        /// Evidence text.
        evidence: String,
    },
    /// Park a poisoned child as a ceiling.
    Park {
        /// Child.
        child: String,
        /// Incarnations spent.
        incarnations: u32,
    },
    /// Page the captain (wedge, dead stop).
    Page {
        /// Count (keys it).
        n: u64,
        /// Kind tag.
        kind: String,
        /// Pin, if any.
        pin: String,
        /// Text.
        line: String,
    },
    /// Record a rejected input as a fact.
    Reject {
        /// Count (keys it).
        n: u64,
        /// Cause.
        cause: RejectCause,
        /// Child, if any.
        child: String,
    },
}

impl KCmd for SupCmd {
    fn durability(&self) -> Durability {
        let key = match self {
            Self::Relaunch { child, generation } => format!("relaunch:{child}:{generation}"),
            Self::Exit { child, generation } => format!("exit:{child}:{generation}"),
            Self::AppendSteer { seq, .. } => format!("steer:{seq}"),
            Self::Escalate { n, reason, .. } => format!("escalate:{n}:{reason}"),
            Self::Park { child, .. } => format!("park:{child}"),
            Self::Page { n, kind, pin, .. } => format!("page:{n}:{kind}:{pin}"),
            Self::Reject { n, cause, .. } => format!("reject:{n}:{}", cause.tag()),
        };
        Durability::Durable { key }
    }
}

/// The Elm program.
pub struct SupervisorProgram;

fn reject(next: &mut SupModel, cmds: &mut Vec<SupCmd>, cause: RejectCause, child: &str) {
    next.rejects += 1;
    cmds.push(SupCmd::Reject {
        n: next.rejects,
        cause,
        child: child.into(),
    });
}

fn escalate(next: &mut SupModel, cmds: &mut Vec<SupCmd>, reason: &str, evidence: String) {
    next.escalations += 1;
    cmds.push(SupCmd::Escalate {
        n: next.escalations,
        reason: reason.into(),
        evidence,
    });
}

/// P11: the restart budget. Window entries older than `max_t` leave; the
/// window is half-open, so an entry exactly `max_t` old has left.
fn window_full(next: &mut SupModel, now: u64) -> bool {
    let max_t = next.policy.max_t_ms;
    next.restarts.retain(|t| now.saturating_sub(*t) < max_t);
    next.restarts.len() as u32 >= next.policy.max_r
}

/// Intensity exceeded: cancel the subtree, fail, escalate once.
fn fail_subtree(next: &mut SupModel, cmds: &mut Vec<SupCmd>, reason: &str, who: &str) {
    let ids: Vec<String> = next
        .children
        .iter()
        .filter(|(_, c)| c.state.live())
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        let Some(c) = next.children.get_mut(&id) else {
            continue;
        };
        c.state = ChildState::Cancelled;
        if id != PASS {
            cmds.push(SupCmd::Exit {
                child: id.clone(),
                generation: c.generation,
            });
        }
    }
    for s in next.steers.values_mut() {
        if s.state == SteerState::Pending {
            s.state = SteerState::Escalated;
        }
    }
    next.status = Status::Failed;
    let ev = format!(
        "{who}: {} restarts inside {}s (max {}); subtree cancelled",
        next.restarts.len(),
        next.policy.max_t_ms / 1000,
        next.policy.max_r
    );
    escalate(next, cmds, reason, ev);
}

/// A child ended abnormally: restart it, park it, or fail the supervisor.
/// `count_incarnation` is false for a deferred restart (herdr was down).
fn abnormal_end(
    next: &mut SupModel,
    cmds: &mut Vec<SupCmd>,
    id: &str,
    now: u64,
    count_incarnation: bool,
) {
    if next.status != Status::Running {
        return;
    }
    let Some(c) = next.children.get_mut(id) else {
        return;
    };
    if !c.state.live() {
        return;
    }
    // Already ended and waiting on herdr: one end is one incarnation.
    if c.state == ChildState::NeedsRestart && count_incarnation {
        return;
    }
    if c.spec.restart == Restart::Temporary {
        c.state = ChildState::Gone;
        return;
    }
    if count_incarnation {
        c.abnormal += 1;
    }
    if c.abnormal >= next.policy.poison {
        c.state = ChildState::Parked;
        let n = c.abnormal;
        for s in next.steers.values_mut() {
            if s.child == id && s.state == SteerState::Pending {
                s.state = SteerState::Escalated;
            }
        }
        cmds.push(SupCmd::Park {
            child: id.into(),
            incarnations: n,
        });
        return;
    }
    if !next.herdr_up {
        c.state = ChildState::NeedsRestart;
        return;
    }
    if window_full(next, now) {
        fail_subtree(next, cmds, "restart_intensity", id);
        return;
    }
    next.restarts.push(now);
    let lease = next.policy.lease_ms;
    let Some(c) = next.children.get_mut(id) else {
        return;
    };
    c.generation += 1;
    c.state = ChildState::Restarting;
    c.paused = false;
    c.lease_deadline_ms = now.saturating_add(lease);
    let generation = c.generation;
    for s in next.steers.values_mut() {
        if s.child == id && s.state == SteerState::Pending {
            s.state = SteerState::AppliedByRestart;
        }
    }
    cmds.push(SupCmd::Relaunch {
        child: id.into(),
        generation,
    });
}

/// A child's report is accepted only at its current generation and with no steer
/// pending (P3, P13). Returns whether it was.
fn accept_report(next: &mut SupModel, cmds: &mut Vec<SupCmd>, id: &str, generation: u64) -> bool {
    if next.status != Status::Running {
        reject(next, cmds, RejectCause::NotRunning, id);
        return false;
    }
    let Some(c) = next.children.get(id) else {
        reject(next, cmds, RejectCause::UnknownChild, id);
        return false;
    };
    if generation != c.generation {
        reject(next, cmds, RejectCause::StaleGen, id);
        return false;
    }
    if !matches!(c.state, ChildState::Running) {
        reject(next, cmds, RejectCause::UnknownChild, id);
        return false;
    }
    if next.pending_steer(id).is_some() {
        reject(next, cmds, RejectCause::SteerPending, id);
        return false;
    }
    true
}

impl Program for SupervisorProgram {
    type Model = SupModel;
    type Msg = SupMsg;
    type Cmd = SupCmd;
    type Err = String;

    fn init(_boot: &Boot) -> Update<SupModel, SupCmd> {
        Update::quiet(SupModel::default())
    }

    fn update(model: &SupModel, msg: &SupMsg) -> Update<SupModel, SupCmd> {
        let mut next = model.clone();
        let mut cmds: Vec<SupCmd> = Vec::new();
        match msg {
            SupMsg::Hello { me, tier, policy } => {
                let mut m = SupModel::new(me, *tier);
                if let Some(p) = policy {
                    m.policy = p.clone();
                }
                next = m;
            }
            SupMsg::Claim { holder, sup_epoch } => {
                if *sup_epoch <= next.accepted_epoch {
                    reject(&mut next, &mut cmds, RejectCause::StaleEpoch, "");
                } else {
                    next.accepted_epoch = *sup_epoch;
                    next.leader = holder.clone();
                    if *holder != next.me {
                        next.status = Status::Superseded;
                    } else if next.status == Status::Superseded {
                        next.status = Status::Running;
                    }
                }
            }
            SupMsg::Spawn { spec, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if next.children.contains_key(&spec.id) {
                    // Idempotent: the registry is re-read every pass.
                } else if spec.id != PASS && next.live_crews() >= next.policy.crew_cap {
                    reject(&mut next, &mut cmds, RejectCause::CapExceeded, &spec.id);
                } else {
                    let order = next.next_order;
                    next.next_order += 1;
                    let lease = now_ms.saturating_add(next.policy.lease_ms);
                    next.children.insert(
                        spec.id.clone(),
                        Child {
                            spec: spec.clone(),
                            state: ChildState::Running,
                            generation: 1,
                            abnormal: 0,
                            lease_deadline_ms: lease,
                            last_progress_ms: *now_ms,
                            order,
                            paused: false,
                        },
                    );
                }
            }
            SupMsg::Progress {
                child,
                generation,
                now_ms,
            } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if accept_report(&mut next, &mut cmds, child, *generation) {
                    let lease = now_ms.saturating_add(next.policy.lease_ms);
                    if let Some(c) = next.children.get_mut(child) {
                        c.lease_deadline_ms = lease;
                        c.last_progress_ms = *now_ms;
                    }
                }
            }
            SupMsg::Done { child, generation, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if accept_report(&mut next, &mut cmds, child, *generation)
                    && let Some(c) = next.children.get_mut(child)
                {
                    c.state = ChildState::Done;
                    c.abnormal = 0;
                }
            }
            SupMsg::Exit { child, now_ms } | SupMsg::Deaf { child, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if next
                    .children
                    .get(child)
                    .is_some_and(|c| c.state == ChildState::Running)
                {
                    abnormal_end(&mut next, &mut cmds, child, *now_ms, true);
                }
            }
            SupMsg::Cursor { child, cursor } => {
                for s in next.steers.values_mut() {
                    if s.child == *child
                        && s.state == SteerState::Pending
                        && s.offset.is_some_and(|o| *cursor >= o)
                    {
                        s.state = SteerState::Acked;
                    }
                }
            }
            SupMsg::Control {
                op,
                child,
                sup_epoch,
                now_ms,
            } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if *sup_epoch < next.accepted_epoch {
                    reject(&mut next, &mut cmds, RejectCause::StaleEpoch, child);
                } else if *sup_epoch > next.accepted_epoch {
                    reject(&mut next, &mut cmds, RejectCause::UnclaimedEpoch, child);
                } else if next.status != Status::Running || next.leader != next.me {
                    reject(&mut next, &mut cmds, RejectCause::NotRunning, child);
                } else if !next.children.get(child).is_some_and(|c| c.state.live()) {
                    reject(&mut next, &mut cmds, RejectCause::UnknownChild, child);
                } else {
                    next.steer_seq += 1;
                    let seq = next.steer_seq;
                    let generation = next.children[child].generation;
                    for s in next.steers.values_mut() {
                        if s.child == *child && s.state == SteerState::Pending {
                            s.state = SteerState::Superseded;
                        }
                    }
                    let ack_by = now_ms.saturating_add(next.policy.ack_ms);
                    next.steers.insert(
                        seq,
                        Steer {
                            seq,
                            child: child.clone(),
                            op: *op,
                            generation,
                            sup_epoch: *sup_epoch,
                            ack_by_ms: ack_by,
                            offset: None,
                            state: SteerState::Pending,
                        },
                    );
                    if let Some(c) = next.children.get_mut(child) {
                        match op {
                            SteerOp::Pause => c.paused = true,
                            SteerOp::Resume => c.paused = false,
                            _ => {}
                        }
                    }
                    cmds.push(SupCmd::AppendSteer {
                        seq,
                        child: child.clone(),
                        op: *op,
                        generation,
                        sup_epoch: *sup_epoch,
                        urgent: *op != SteerOp::Resume,
                    });
                }
            }
            SupMsg::SteerAppended { seq, offset } => {
                if let Some(s) = next.steers.get_mut(seq) {
                    s.offset = Some(*offset);
                }
            }
            SupMsg::Relaunched { child, generation } => {
                if let Some(c) = next.children.get_mut(child)
                    && c.state == ChildState::Restarting
                    && c.generation == *generation
                {
                    c.state = ChildState::Running;
                }
            }
            SupMsg::HerdrDown { now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                next.herdr_up = false;
                if !next.herdr_escalated {
                    next.herdr_escalated = true;
                    escalate(
                        &mut next,
                        &mut cmds,
                        "herdr_down",
                        "herdr unreachable: log-only, no restarts until it is back".into(),
                    );
                }
            }
            SupMsg::HerdrUp { now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                next.herdr_up = true;
                next.herdr_escalated = false;
            }
            SupMsg::PassExit { ok, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if *ok {
                    if let Some(c) = next.children.get_mut(PASS) {
                        c.abnormal = 0;
                    }
                } else {
                    // The failed pass is one restart of the pass child; the
                    // host's next interval is the relaunch.
                    abnormal_end(&mut next, &mut cmds, PASS, *now_ms, false);
                }
            }
            SupMsg::Wedged { pin, line } => {
                if next.paged.insert(pin.clone()) {
                    next.pages += 1;
                    cmds.push(SupCmd::Page {
                        n: next.pages,
                        kind: "wedge".into(),
                        pin: pin.clone(),
                        line: line.clone(),
                    });
                }
            }
            SupMsg::Stalled { line } => {
                if !next.stalled {
                    next.stalled = true;
                    next.pages += 1;
                    cmds.push(SupCmd::Page {
                        n: next.pages,
                        kind: "dead_stop".into(),
                        pin: String::new(),
                        line: line.clone(),
                    });
                }
            }
            SupMsg::WorkMoved => {
                next.paged.clear();
                next.stalled = false;
            }
            SupMsg::Tick { now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                let now = next.now_ms;
                if next.status == Status::Running {
                    // A restart herdr's absence deferred (level-triggered).
                    if next.herdr_up {
                        let ids: Vec<String> = next
                            .children
                            .iter()
                            .filter(|(_, c)| c.state == ChildState::NeedsRestart)
                            .map(|(id, _)| id.clone())
                            .collect();
                        for id in ids {
                            abnormal_end(&mut next, &mut cmds, &id, now, false);
                        }
                    }
                    // Steers not acked by their deadline (P10).
                    let due: Vec<u64> = next
                        .steers
                        .values()
                        .filter(|s| s.state == SteerState::Pending && now > s.ack_by_ms)
                        .map(|s| s.seq)
                        .collect();
                    for seq in due {
                        let Some(s) = next.steers.get(&seq).cloned() else {
                            continue;
                        };
                        if s.op != SteerOp::Cancel
                            && next
                                .children
                                .get(&s.child)
                                .is_none_or(|c| c.state != ChildState::Running)
                        {
                            continue;
                        }
                        if s.op == SteerOp::Cancel {
                            if let Some(c) = next.children.get_mut(&s.child)
                                && c.state.live()
                            {
                                c.state = ChildState::Cancelled;
                                cmds.push(SupCmd::Exit {
                                    child: s.child.clone(),
                                    generation: c.generation,
                                });
                            }
                            if let Some(st) = next.steers.get_mut(&seq) {
                                st.state = SteerState::AppliedByRestart;
                            }
                        } else {
                            // abnormal_end settles the steer: applied by
                            // restart, or escalated if the budget is spent.
                            abnormal_end(&mut next, &mut cmds, &s.child, now, true);
                            if let Some(st) = next.steers.get_mut(&seq)
                                && st.state == SteerState::Pending
                            {
                                st.state = SteerState::Escalated;
                            }
                        }
                    }
                    // Lease expiry (strictly past the deadline).
                    let expired: Vec<String> = next
                        .children
                        .iter()
                        .filter(|(_, c)| {
                            matches!(c.state, ChildState::Running | ChildState::Restarting)
                                && !c.paused
                                && c.spec.id != PASS
                                && now > c.lease_deadline_ms
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in expired {
                        abnormal_end(&mut next, &mut cmds, &id, now, true);
                    }
                }
            }
            SupMsg::CmdFailed { key, why: _ } => {
                next.failed_cmds += 1;
                // A relaunch that did not land is another failed incarnation
                // of the same child: it spends the budget, never loops free.
                if let Some(rest) = key.strip_prefix("relaunch:")
                    && let Some((id, generation)) = rest.rsplit_once(':')
                    && let Ok(generation) = generation.parse::<u64>()
                    && next
                        .children
                        .get(id)
                        .is_some_and(|c| c.state == ChildState::Restarting && c.generation == generation)
                {
                    let now = next.now_ms;
                    abnormal_end(&mut next, &mut cmds, id, now, true);
                }
            }
        }
        Update::with_commands(next, cmds)
    }

    fn failure(f: &Failure<String>) -> SupMsg {
        SupMsg::CmdFailed {
            key: String::new(),
            why: format!("{f:?}"),
        }
    }
}

/// One row of the monitoring view (§6A.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewRow {
    /// Child id.
    pub child: String,
    /// State.
    pub state: ChildState,
    /// Incarnation.
    pub generation: u64,
    /// Abnormal incarnations.
    pub abnormal: u32,
    /// Lease deadline.
    pub lease_deadline_ms: u64,
    /// Unacked steer, if any: (seq, op).
    pub pending_steer: Option<(u64, SteerOp)>,
}

/// `CrewView`: the projection operators and the helm read. Ordered by
/// start order, so two replays of one log print the same view.
pub fn crew_view(m: &SupModel) -> Vec<CrewRow> {
    let mut rows: Vec<(&Child, CrewRow)> = m
        .children
        .values()
        .map(|c| {
            (
                c,
                CrewRow {
                    child: c.spec.id.clone(),
                    state: c.state,
                    generation: c.generation,
                    abnormal: c.abnormal,
                    lease_deadline_ms: c.lease_deadline_ms,
                    pending_steer: m.pending_steer(&c.spec.id).map(|s| (s.seq, s.op)),
                },
            )
        })
        .collect();
    rows.sort_by_key(|(c, _)| c.order);
    rows.into_iter().map(|(_, r)| r).collect()
}

/// Fold a log from a fresh model (replay). Returns the model and every
/// command the fold emitted, in order.
pub fn replay(msgs: &[SupMsg]) -> (SupModel, Vec<SupCmd>) {
    let mut m = SupModel::default();
    let mut all = Vec::new();
    for msg in msgs {
        let u = SupervisorProgram::update(&m, msg);
        m = u.model;
        all.extend(u.commands);
    }
    (m, all)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "lead-1";
    const MIN: u64 = 60_000;

    fn spec(id: &str) -> ChildSpec {
        ChildSpec {
            id: id.into(),
            npub: Some(format!("npub-{id}")),
            restart: Restart::Transient,
        }
    }

    /// A lead with a claimed lease at epoch 2 and crews `a`, `b` spawned at t=0.
    fn base() -> SupModel {
        let mut m = SupModel::default();
        for msg in [
            SupMsg::Hello {
                me: ME.into(),
                tier: Tier::Lead,
                policy: None,
            },
            SupMsg::Claim {
                holder: ME.into(),
                sup_epoch: 2,
            },
            SupMsg::Spawn {
                spec: spec("a"),
                now_ms: 0,
            },
            SupMsg::Spawn {
                spec: spec("b"),
                now_ms: 0,
            },
        ] {
            m = step(&m, &msg).0;
        }
        m
    }

    fn step(m: &SupModel, msg: &SupMsg) -> (SupModel, Vec<SupCmd>) {
        let u = SupervisorProgram::update(m, msg);
        (u.model, u.commands)
    }

    fn run(m: SupModel, msgs: &[SupMsg]) -> (SupModel, Vec<SupCmd>) {
        let mut m = m;
        let mut all = Vec::new();
        for x in msgs {
            let (n, c) = step(&m, x);
            m = n;
            all.extend(c);
        }
        (m, all)
    }

    fn exit(id: &str, t: u64) -> SupMsg {
        SupMsg::Exit {
            child: id.into(),
            now_ms: t,
        }
    }

    fn relaunches(c: &[SupCmd]) -> Vec<(String, u64)> {
        c.iter()
            .filter_map(|c| match c {
                SupCmd::Relaunch { child, generation } => Some((child.clone(), *generation)),
                _ => None,
            })
            .collect()
    }

    fn escalations(c: &[SupCmd]) -> usize {
        c.iter()
            .filter(|c| matches!(c, SupCmd::Escalate { .. }))
            .count()
    }

    fn rejects(c: &[SupCmd]) -> Vec<RejectCause> {
        c.iter()
            .filter_map(|c| match c {
                SupCmd::Reject { cause, .. } => Some(*cause),
                _ => None,
            })
            .collect()
    }

    /// Crash `id` then confirm the relaunch landed.
    fn crash_and_relaunch(m: SupModel, id: &str, t: u64) -> (SupModel, Vec<SupCmd>) {
        let (m, c) = step(&m, &exit(id, t));
        let generation = m.children[id].generation;
        let (m, _) = step(
            &m,
            &SupMsg::Relaunched {
                child: id.into(),
                generation,
            },
        );
        (m, c)
    }

    // ---- defaults (build-plan: leads 3/30 min, captain 2/60 min, poison 3, cap 5)

    #[test]
    fn the_settled_defaults_are_the_policy() {
        let l = Policy::for_tier(Tier::Lead);
        assert_eq!((l.max_r, l.max_t_ms), (3, 30 * MIN));
        let c = Policy::for_tier(Tier::Captain);
        assert_eq!((c.max_r, c.max_t_ms), (2, 60 * MIN));
        assert_eq!((l.poison, l.crew_cap), (3, 5));
        assert_eq!(l.strategy, Strategy::OneForOne);
    }

    // ---- P11: restart intensity

    #[test]
    fn four_failures_in_the_window_escalate_after_three_restarts() {
        let mut m = base();
        let mut restarts = 0;
        for (i, id) in ["a", "b", "a"].into_iter().enumerate() {
            let (n, c) = crash_and_relaunch(m, id, 1_000 + i as u64 * 1_000);
            m = n;
            restarts += relaunches(&c).len();
            assert_eq!(escalations(&c), 0, "failure {} restarts", i + 1);
        }
        assert_eq!(restarts, 3);
        // Alternating crews keeps every child under the poison ceiling, so the
        // 4th failure pins the intensity rule alone.
        let (m, c) = step(&m, &exit("b", 5_000));
        assert!(relaunches(&c).is_empty(), "the 4th restart is not made");
        assert_eq!(escalations(&c), 1);
        assert_eq!(m.status, Status::Failed);
        assert!(m.children.values().all(|c| !c.state.live()), "subtree cancelled");
    }

    #[test]
    fn captain_budget_is_two_per_sixty_minutes() {
        let mut m = SupModel::default();
        m = step(
            &m,
            &SupMsg::Hello {
                me: ME.into(),
                tier: Tier::Captain,
                policy: None,
            },
        )
        .0;
        m = step(&m, &SupMsg::Claim { holder: ME.into(), sup_epoch: 1 }).0;
        for id in ["a", "b", "c"] {
            m = step(&m, &SupMsg::Spawn { spec: spec(id), now_ms: 0 }).0;
        }
        let (m, c1) = step(&m, &exit("a", 1_000));
        let (m, c2) = step(&m, &exit("b", 2_000));
        let (m, c3) = step(&m, &exit("c", 3_000));
        assert_eq!(relaunches(&c1).len() + relaunches(&c2).len(), 2);
        assert!(relaunches(&c3).is_empty());
        assert_eq!(escalations(&c3), 1);
        assert_eq!(m.status, Status::Failed);
    }

    #[test]
    fn the_window_is_half_open_at_max_t() {
        // 3 restarts at t=0, 1, 2 s. A failure exactly 30 min after the first
        // finds that one gone (age == MaxT is outside); one ms earlier it is not.
        let mk = |t_fail: u64| {
            let mut m = base();
            for t in [0u64, 1_000, 2_000] {
                m.restarts.push(t);
            }
            step(&m, &exit("b", t_fail))
        };
        let (m_in, c_in) = mk(LEAD_MAX_T_MS - 1);
        assert_eq!(escalations(&c_in), 1, "age MaxT-1 still counts");
        assert_eq!(m_in.status, Status::Failed);
        let (m_out, c_out) = mk(LEAD_MAX_T_MS);
        assert_eq!(escalations(&c_out), 0, "age == MaxT has left the window");
        assert_eq!(relaunches(&c_out), vec![("b".to_string(), 2)]);
        assert_eq!(m_out.status, Status::Running);
    }

    #[test]
    fn nothing_restarts_after_the_supervisor_failed() {
        let mut m = base();
        m = step(&m, &SupMsg::Spawn { spec: ChildSpec { id: PASS.into(), npub: None, restart: Restart::Permanent }, now_ms: 0 }).0;
        let mut esc = 0;
        for t in 1..=6u64 {
            let (n, c) = step(&m, &SupMsg::PassExit { ok: false, now_ms: t * 1_000 });
            m = n;
            esc += escalations(&c);
        }
        assert_eq!(esc, 1, "one escalation, then silence");
        assert_eq!(m.status, Status::Failed);
        let (_, c) = step(&m, &exit("a", 9_000));
        assert!(relaunches(&c).is_empty());
    }

    // ---- P3/P10: generation fencing

    #[test]
    fn a_late_done_from_a_replaced_incarnation_is_fenced() {
        let (m, _) = crash_and_relaunch(base(), "a", 1_000);
        assert_eq!(m.children["a"].generation, 2);
        let (m2, c) = step(
            &m,
            &SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: 2_000,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleGen]);
        assert_eq!(m2.children["a"].state, ChildState::Running, "not done");
        let (m3, c) = step(
            &m2,
            &SupMsg::Done {
                child: "a".into(),
                generation: 2,
                now_ms: 3_000,
            },
        );
        assert!(rejects(&c).is_empty());
        assert_eq!(m3.children["a"].state, ChildState::Done);
    }

    #[test]
    fn progress_at_a_stale_gen_does_not_renew_the_lease() {
        let (m, _) = crash_and_relaunch(base(), "a", 1_000);
        let before = m.children["a"].lease_deadline_ms;
        let (m, c) = step(
            &m,
            &SupMsg::Progress {
                child: "a".into(),
                generation: 1,
                now_ms: 50_000,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleGen]);
        assert_eq!(m.children["a"].lease_deadline_ms, before);
    }

    // ---- leases

    #[test]
    fn a_lease_expires_strictly_past_its_deadline_and_restarts_at_the_next_gen() {
        let m = base();
        let dl = m.children["a"].lease_deadline_ms;
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: dl });
        assert!(relaunches(&c).is_empty(), "at the deadline the lease holds");
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: dl + 1 });
        assert_eq!(
            relaunches(&c),
            vec![("a".to_string(), 2), ("b".to_string(), 2)]
        );
        assert_eq!(m.children["a"].state, ChildState::Restarting);
    }

    #[test]
    fn progress_renews_the_lease() {
        let m = base();
        let (m, _) = step(
            &m,
            &SupMsg::Progress {
                child: "a".into(),
                generation: 1,
                now_ms: 500_000,
            },
        );
        let (_, c) = step(&m, &SupMsg::Tick { now_ms: DEFAULT_LEASE_MS + 10 });
        assert_eq!(relaunches(&c), vec![("b".to_string(), 2)], "a was renewed, b was not");
    }

    // ---- P9: sup_epoch fence

    #[test]
    fn a_control_below_the_accepted_epoch_is_rejected() {
        let m = base();
        let ctl = |e| SupMsg::Control {
            op: SteerOp::Cancel,
            child: "a".into(),
            sup_epoch: e,
            now_ms: 10,
        };
        let (m1, c) = step(&m, &ctl(1));
        assert_eq!(rejects(&c), vec![RejectCause::StaleEpoch]);
        assert!(m1.steers.is_empty());
        let (m2, c) = step(&m, &ctl(2));
        assert!(rejects(&c).is_empty());
        assert_eq!(m2.steers.len(), 1);
        let (_, c) = step(&m, &ctl(3));
        assert_eq!(rejects(&c), vec![RejectCause::UnclaimedEpoch]);
    }

    #[test]
    fn a_higher_claim_supersedes_this_supervisor_and_it_acts_on_nothing() {
        let (m, c) = step(
            &base(),
            &SupMsg::Claim {
                holder: "lead-2".into(),
                sup_epoch: 3,
            },
        );
        assert!(c.is_empty());
        assert_eq!(m.status, Status::Superseded);
        let (m, c) = run(m, &[exit("a", 1), SupMsg::Tick { now_ms: DEFAULT_LEASE_MS * 2 }]);
        assert!(relaunches(&c).is_empty(), "a superseded supervisor restarts nothing");
        assert_eq!(m.children["a"].state, ChildState::Running);
        // And its own old epoch is now stale.
        let (_, c) = step(
            &m,
            &SupMsg::Control {
                op: SteerOp::Cancel,
                child: "a".into(),
                sup_epoch: 2,
                now_ms: 5,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleEpoch]);
    }

    #[test]
    fn a_claim_at_or_below_the_accepted_epoch_is_rejected() {
        let (_, c) = step(
            &base(),
            &SupMsg::Claim {
                holder: "x".into(),
                sup_epoch: 2,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleEpoch]);
    }

    // ---- P13: the steer gate; P10: resolution

    fn steered() -> SupModel {
        let (m, c) = step(
            &base(),
            &SupMsg::Control {
                op: SteerOp::Redirect,
                child: "a".into(),
                sup_epoch: 2,
                now_ms: 1_000,
            },
        );
        assert!(matches!(
            c.as_slice(),
            [SupCmd::AppendSteer { seq: 1, urgent: true, generation: 1, .. }]
        ));
        step(&m, &SupMsg::SteerAppended { seq: 1, offset: 7 }).0
    }

    #[test]
    fn while_a_steer_is_unacked_no_progress_or_done_is_accepted() {
        let m = steered();
        for msg in [
            SupMsg::Progress { child: "a".into(), generation: 1, now_ms: 2_000 },
            SupMsg::Done { child: "a".into(), generation: 1, now_ms: 2_000 },
        ] {
            let (m2, c) = step(&m, &msg);
            assert_eq!(rejects(&c), vec![RejectCause::SteerPending]);
            assert_eq!(m2.children["a"].state, ChildState::Running);
        }
        // The other child is not gated.
        let (_, c) = step(&m, &SupMsg::Progress { child: "b".into(), generation: 1, now_ms: 2_000 });
        assert!(rejects(&c).is_empty());
    }

    #[test]
    fn the_cursor_reaching_the_steer_offset_acks_it_and_opens_the_gate() {
        let m = steered();
        let (m, _) = step(&m, &SupMsg::Cursor { child: "a".into(), cursor: 6 });
        assert_eq!(m.steers[&1].state, SteerState::Pending, "6 < 7");
        let (m, _) = step(&m, &SupMsg::Cursor { child: "a".into(), cursor: 7 });
        assert_eq!(m.steers[&1].state, SteerState::Acked);
        let (_, c) = step(&m, &SupMsg::Done { child: "a".into(), generation: 1, now_ms: 3_000 });
        assert!(rejects(&c).is_empty());
    }

    #[test]
    fn a_cursor_of_another_child_does_not_ack() {
        let m = steered();
        let (m, _) = step(&m, &SupMsg::Cursor { child: "b".into(), cursor: 99 });
        assert_eq!(m.steers[&1].state, SteerState::Pending);
    }

    #[test]
    fn a_crew_ignoring_a_steer_is_restarted_at_the_next_gen_and_its_late_done_is_fenced() {
        let m = steered();
        let ack_by = m.steers[&1].ack_by_ms;
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: ack_by });
        assert!(relaunches(&c).is_empty(), "at ack_by the crew still has time");
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: ack_by + 1 });
        assert_eq!(relaunches(&c), vec![("a".to_string(), 2)]);
        assert_eq!(m.steers[&1].state, SteerState::AppliedByRestart);
        let (_, c) = step(&m, &SupMsg::Done { child: "a".into(), generation: 1, now_ms: ack_by + 2 });
        assert_eq!(rejects(&c), vec![RejectCause::StaleGen]);
    }

    #[test]
    fn an_ignored_cancel_ends_the_child_instead_of_restarting_it() {
        let (m, _) = step(
            &base(),
            &SupMsg::Control { op: SteerOp::Cancel, child: "a".into(), sup_epoch: 2, now_ms: 0 },
        );
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: DEFAULT_ACK_MS + 1 });
        assert!(relaunches(&c).iter().all(|(id, _)| id != "a"));
        assert!(c.contains(&SupCmd::Exit { child: "a".into(), generation: 1 }));
        assert_eq!(m.children["a"].state, ChildState::Cancelled);
        assert_eq!(m.steers[&1].state, SteerState::AppliedByRestart);
    }

    #[test]
    fn a_newer_steer_supersedes_the_pending_one() {
        let m = steered();
        let (m, _) = step(
            &m,
            &SupMsg::Control { op: SteerOp::Pause, child: "a".into(), sup_epoch: 2, now_ms: 2_000 },
        );
        assert_eq!(m.steers[&1].state, SteerState::Superseded);
        assert_eq!(m.steers[&2].state, SteerState::Pending);
        assert!(m.children["a"].paused);
    }

    #[test]
    fn a_steer_whose_restart_budget_is_spent_is_escalated() {
        let mut m = steered();
        for t in [0u64, 1, 2] {
            m.restarts.push(t);
        }
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: m.steers[&1].ack_by_ms + 1 });
        assert_eq!(escalations(&c), 1);
        assert_eq!(m.steers[&1].state, SteerState::Escalated);
    }

    // ---- poison, restart kinds, cap, herdr down

    #[test]
    fn three_abnormal_incarnations_park_the_child_as_a_ceiling() {
        let mut m = base();
        let mut parks = 0;
        for t in 0..3u64 {
            let (n, c) = crash_and_relaunch(m, "a", 1_000 * (t + 1));
            m = n;
            parks += c.iter().filter(|c| matches!(c, SupCmd::Park { .. })).count();
        }
        assert_eq!(parks, 1);
        assert_eq!(m.children["a"].state, ChildState::Parked);
    }

    #[test]
    fn a_normal_end_is_final_and_a_temporary_child_is_never_restarted() {
        let (m, _) = step(&base(), &SupMsg::Done { child: "a".into(), generation: 1, now_ms: 5 });
        let (m, c) = step(&m, &exit("a", 6));
        assert!(relaunches(&c).is_empty(), "Done is final");
        assert_eq!(m.children["a"].state, ChildState::Done);
        let mut m = base();
        m = step(
            &m,
            &SupMsg::Spawn {
                spec: ChildSpec { id: "t".into(), npub: None, restart: Restart::Temporary },
                now_ms: 0,
            },
        )
        .0;
        let (m, c) = step(&m, &exit("t", 9));
        assert!(relaunches(&c).is_empty());
        assert_eq!(m.children["t"].state, ChildState::Gone);
    }

    #[test]
    fn the_crew_cap_is_five_panes() {
        let mut m = base();
        for id in ["c", "d", "e"] {
            m = step(&m, &SupMsg::Spawn { spec: spec(id), now_ms: 0 }).0;
        }
        assert_eq!(m.live_crews(), 5);
        let (m, c) = step(&m, &SupMsg::Spawn { spec: spec("f"), now_ms: 0 });
        assert_eq!(rejects(&c), vec![RejectCause::CapExceeded]);
        assert!(!m.children.contains_key("f"));
        // Spawn is idempotent: re-reading the registry adds nothing.
        let (m2, c) = step(&m, &SupMsg::Spawn { spec: spec("a"), now_ms: 9 });
        assert!(c.is_empty() && m2.children == m.children);
    }

    #[test]
    fn with_herdr_down_nothing_restarts_and_one_escalation_goes_out_then_it_catches_up() {
        let (m, c) = step(&base(), &SupMsg::HerdrDown { now_ms: 10 });
        assert_eq!(escalations(&c), 1);
        let (m, c) = step(&m, &SupMsg::HerdrDown { now_ms: 11 });
        assert_eq!(escalations(&c), 0, "once per outage");
        let (m, c) = step(&m, &exit("a", 20));
        assert!(relaunches(&c).is_empty(), "log-only while herdr is down");
        assert_eq!(m.children["a"].state, ChildState::NeedsRestart);
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: 30 });
        assert!(relaunches(&c).is_empty(), "still down at the tick");
        let (m, _) = step(&m, &SupMsg::HerdrUp { now_ms: 40 });
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: 41 });
        assert_eq!(relaunches(&c), vec![("a".to_string(), 2)]);
        assert_eq!(m.children["a"].abnormal, 1, "one end is one incarnation");
        assert_eq!(m.restarts.len(), 1, "and it went through the intensity check");
    }

    #[test]
    fn a_relaunch_that_failed_spends_the_budget_and_never_loops_free() {
        let (mut m, _) = step(&base(), &exit("a", 1_000));
        let mut esc = 0;
        for i in 0..6u64 {
            let key = format!("relaunch:a:{}", m.children["a"].generation);
            let (n, c) = step(&m, &SupMsg::CmdFailed { key, why: "x".into() });
            m = n;
            esc += escalations(&c);
            if !matches!(m.children["a"].state, ChildState::Restarting) {
                break;
            }
            let _ = i;
        }
        assert!(
            m.children["a"].state == ChildState::Parked || esc == 1,
            "bounded: parked by poison or escalated by intensity"
        );
    }

    // ---- the pass child, wedge pages, dead stop (the locals cmd_loop_supervise kept)

    #[test]
    fn a_wedge_pages_once_until_work_moves() {
        let w = |p: &str| SupMsg::Wedged { pin: p.into(), line: "wedged".into() };
        let m = base();
        let (m, c1) = step(&m, &w("a"));
        let (m, c2) = step(&m, &w("a"));
        let (m, c3) = step(&m, &w("b"));
        assert_eq!((c1.len(), c2.len(), c3.len()), (1, 0, 1));
        let (m, _) = step(&m, &SupMsg::WorkMoved);
        let (_, c4) = step(&m, &w("a"));
        assert_eq!(c4.len(), 1, "a later wedge in the same pane pages again");
    }

    #[test]
    fn a_dead_stop_pages_once_per_stall() {
        let s = SupMsg::Stalled { line: "dead stop".into() };
        let (m, c1) = step(&base(), &s);
        let (m, c2) = step(&m, &s);
        assert_eq!((c1.len(), c2.len()), (1, 0));
        let (m, _) = step(&m, &SupMsg::WorkMoved);
        let (_, c3) = step(&m, &s);
        assert_eq!(c3.len(), 1);
    }

    #[test]
    fn a_successful_pass_resets_the_pass_childs_incarnations() {
        let mut m = base();
        m = step(&m, &SupMsg::Spawn { spec: ChildSpec { id: PASS.into(), npub: None, restart: Restart::Permanent }, now_ms: 0 }).0;
        m = step(&m, &SupMsg::PassExit { ok: false, now_ms: 1_000 }).0;
        assert_eq!(m.restarts.len(), 1);
        m = step(&m, &SupMsg::PassExit { ok: true, now_ms: 2_000 }).0;
        assert_eq!(m.children[PASS].abnormal, 0);
    }

    // ---- P8: replay; the view; serialization

    fn script() -> Vec<SupMsg> {
        vec![
            SupMsg::Hello { me: ME.into(), tier: Tier::Lead, policy: None },
            SupMsg::Claim { holder: ME.into(), sup_epoch: 2 },
            SupMsg::Spawn { spec: spec("a"), now_ms: 0 },
            SupMsg::Spawn { spec: spec("b"), now_ms: 0 },
            SupMsg::Control { op: SteerOp::Redirect, child: "a".into(), sup_epoch: 2, now_ms: 1_000 },
            SupMsg::SteerAppended { seq: 1, offset: 4 },
            exit("b", 2_000),
            SupMsg::Relaunched { child: "b".into(), generation: 2 },
            SupMsg::Progress { child: "b".into(), generation: 2, now_ms: 3_000 },
            SupMsg::Control { op: SteerOp::Cancel, child: "b".into(), sup_epoch: 1, now_ms: 3_500 },
            SupMsg::Tick { now_ms: 4_000 },
        ]
    }

    #[test]
    fn replay_gives_the_same_child_set_pending_steers_and_view() {
        let log = script();
        let (full, _) = replay(&log);
        assert_eq!(full.children.len(), 2);
        assert_eq!(full.pending_steer("a").map(|s| s.seq), Some(1));
        // Kill after every prefix: snapshot, restore, finish the log.
        for k in 0..log.len() {
            let (head, _) = replay(&log[..k]);
            let snap = serde_json::to_string(&head).unwrap();
            let mut m: SupModel = serde_json::from_str(&snap).unwrap();
            for msg in &log[k..] {
                m = step(&m, msg).0;
            }
            assert_eq!(m, full, "cut at {k}");
            assert_eq!(crew_view(&m), crew_view(&full));
        }
        // Same log, same view, twice.
        assert_eq!(crew_view(&replay(&log).0), crew_view(&full));
        assert_eq!(
            crew_view(&full).iter().map(|r| r.child.as_str()).collect::<Vec<_>>(),
            ["a", "b"],
            "ordered by start order"
        );
    }

    #[test]
    fn every_message_and_command_round_trips_through_json() {
        for m in script() {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<SupMsg>(&s).unwrap(), m);
        }
        let (_, cmds) = replay(&script());
        assert!(!cmds.is_empty());
        for c in cmds {
            let s = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<SupCmd>(&s).unwrap(), c);
            assert!(matches!(c.durability(), Durability::Durable { .. }));
        }
    }

    #[test]
    fn durable_keys_are_stable_per_intent() {
        let k = |c: &SupCmd| match c.durability() {
            Durability::Durable { key } => key,
            Durability::Ephemeral => String::new(),
        };
        assert_eq!(k(&SupCmd::Relaunch { child: "a".into(), generation: 3 }), "relaunch:a:3");
        assert_eq!(
            k(&SupCmd::AppendSteer {
                seq: 4,
                child: "a".into(),
                op: SteerOp::Cancel,
                generation: 1,
                sup_epoch: 2,
                urgent: true
            }),
            "steer:4"
        );
    }
}

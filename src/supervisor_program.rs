// governed-by: redesign-proposal §6A (supervisor topology), build-plan §8 (Phase 4a)
//! `SupervisorProgram`: the pure fold behind `loop-mode supervise` (Phase 4a).
//!
//! `(Model, Msg) -> (Model, Vec<Cmd>)`, no clock, no store, no unordered
//! maps, no shell-out. Time arrives as `now_ms` on a message. The host
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
/// Floor for the orphan window. A lead is adoptable only after
/// `max(2 * lease_ms, ORPHAN_FLOOR_MS)` with no progress.
pub const ORPHAN_FLOOR_MS: u64 = 600_000;
/// The loop pass child's id.
pub const PASS: &str = "pass";

/// Orphan window for a lease. Not read from the environment: the host puts
/// the result on [`Policy`] before the fold sees it.
pub fn orphan_window_ms(lease_ms: u64) -> u64 {
    lease_ms.saturating_mul(2).max(ORPHAN_FLOOR_MS)
}

/// The window the host writes onto [`Policy::t_orphan_ms`]. `scale` is
/// `FLYWHEEL_SUPERVISOR_CLOCK_SCALE` (1 outside a fixture). The fold does
/// not read the environment. A scale of 1 is the real window. A fixture
/// scale shrinks it so a test can pass `now_ms` without sleeping minutes.
pub fn scaled_orphan_ms(lease_ms: u64, scale: u64) -> u64 {
    let raw = orphan_window_ms(lease_ms);
    (raw / scale.max(1)).max(1)
}

/// One `Adopt` for a dead lead, or nothing. Nothing before `t_orphan_ms`,
/// nothing while the host's pid is alive, nothing for a crew, nothing twice.
pub fn adopt_msg(model: &SupModel, child: &str, now_ms: u64, host_dead: bool) -> Option<SupMsg> {
    if !host_dead || model.adopted.contains(child) {
        return None;
    }
    let c = model.children.get(child)?;
    if c.spec.kind != ChildKind::Lead || c.state != ChildState::Running {
        return None;
    }
    if now_ms.saturating_sub(c.last_progress_ms) < model.policy.t_orphan_ms {
        return None;
    }
    let sup_epoch = model.accepted_epoch.saturating_add(1);
    if sup_epoch <= model.accepted_epoch {
        return None;
    }
    Some(SupMsg::Adopt {
        child: child.to_string(),
        new_parent: model.me.clone(),
        sup_epoch,
        now_ms,
    })
}

fn default_t_orphan_ms() -> u64 {
    orphan_window_ms(DEFAULT_LEASE_MS)
}

/// Which tier this supervisor is (picks the intensity budget).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A plane lead (`loop-mode supervise`): 3 per 30 min.
    Lead,
    /// The captain (root): 2 per 60 min.
    Captain,
}

/// Crew pane, or one nested lead process. A lead is one child of this
/// supervisor: its own crews stay in its own model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildKind {
    /// A pane. Old logs omit `kind` and parse as this.
    #[default]
    Crew,
    /// A nested `loop-mode supervise` context. Counts toward `crew_cap`.
    Lead,
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

/// What a failure restarts (OTP's strategies, §6A.2). Data: the intensity
/// code does not look at it, only [`restart_set`] does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Only the failed child.
    OneForOne,
    /// The failed child and every live child started after it (a planner ->
    /// coder -> reviewer chain: a restarted planner invalidates what follows).
    RestForOne,
    /// Every live child.
    OneForAll,
}

impl Strategy {
    /// The word fixtures and `FLYWHEEL_SUPERVISOR_STRATEGY` use.
    pub fn parse(w: &str) -> Option<Self> {
        match w {
            "one_for_one" => Some(Self::OneForOne),
            "rest_for_one" => Some(Self::RestForOne),
            "one_for_all" => Some(Self::OneForAll),
            _ => None,
        }
    }
}

/// How many proved children satisfy the goal (§6A.6).
///
/// `All` waits for every non-pass child. `Quorum` and `First` cancel the
/// rest once `k` children are proved. `k` of 0 is refused at parse time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Join {
    /// Every non-pass child must be proved.
    #[default]
    All,
    /// `k` of the children, then cancel the rest.
    Quorum(u32),
    /// The first `k` proved children, then cancel the rest.
    First(u32),
}

impl Join {
    /// `all`, `quorum:N`, or `first:N`. `N` must be at least 1.
    pub fn parse(w: &str) -> Option<Self> {
        if w == "all" {
            return Some(Self::All);
        }
        let (kind, n) = w.split_once(':')?;
        let n: u32 = n.parse().ok()?;
        if n == 0 {
            return None;
        }
        match kind {
            "quorum" => Some(Self::Quorum(n)),
            "first" => Some(Self::First(n)),
            _ => None,
        }
    }

    fn cancels_rest(self) -> bool {
        matches!(self, Self::Quorum(_) | Self::First(_))
    }
}

/// A sibling grant the supervisor recorded. Toron still refuses the
/// append that would change another crew's task. This row is the grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRow {
    /// One side.
    pub a: String,
    /// The other side.
    pub b: String,
    /// Verbs the grant allows, as the operator wrote them.
    pub verbs: String,
    /// When the grant stops counting. `0` means the log did not say.
    pub until_ms: u64,
}

/// Self-reported usage. Never a hard cost gate: there is no trusted meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Input tokens the child reported.
    pub input_tokens: u64,
    /// Output tokens the child reported.
    pub output_tokens: u64,
    /// Micro-dollars the child reported. Advisory only.
    pub usd_micros: u64,
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
    /// How long a lead may go without progress before another parent may
    /// adopt it. `max(2 * lease_ms, ORPHAN_FLOOR_MS)`, set by [`Policy::for_tier`].
    #[serde(default = "default_t_orphan_ms")]
    pub t_orphan_ms: u64,
    /// Which proved children satisfy the goal. Missing on old logs: [`Join::All`].
    #[serde(default)]
    pub join: Join,
    /// Wall clock from the first tick that sees it. `None` does not gate.
    /// Cost is never this field: self-reported usage stays advisory.
    #[serde(default)]
    pub max_wall_ms: Option<u64>,
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
            t_orphan_ms: orphan_window_ms(DEFAULT_LEASE_MS),
            join: Join::All,
            max_wall_ms: None,
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
    /// Crew or lead. Missing on old logs: [`ChildKind::Crew`].
    #[serde(default)]
    pub kind: ChildKind,
    /// Beads whose `role` matches this are eligible. Missing means the child id.
    #[serde(default)]
    pub role: Option<String>,
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
    /// Cancelled by a steer. Final.
    Cancelled,
    /// Stopped by a subtree cancel (restart intensity exceeded). Final for
    /// this incarnation; the next incarnation re-adopts it from the registry.
    Halted,
    /// Poison ceiling reached. Final until an operator acts.
    Parked,
    /// Ended and not restarted (temporary). Final.
    Gone,
}

impl ChildState {
    fn live(self) -> bool {
        matches!(self, Self::Running | Self::Restarting | Self::NeedsRestart)
    }

    /// Whether the host should keep driving this child (pass cadence).
    pub fn live_for_host(self) -> bool {
        self.live()
    }
}

/// One bead the supervisor may hand to a crew.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyBead {
    /// Task id.
    pub id: String,
    /// Paths the task reserves. Overlap with an in-flight assignment waits.
    pub scope_paths: Vec<String>,
    /// Dependencies proved. `false` is not assigned.
    pub deps_proved: bool,
    /// Must equal the child's id, or its `role` when set.
    pub role: String,
}

/// An in-flight assignment. The fold records it; the host does not write a pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    /// Task id.
    pub task: String,
    /// Crew it was given to.
    pub child: String,
    /// That child's generation at assignment.
    pub generation: u64,
    /// Scope held while this assignment is in flight.
    pub scope_paths: Vec<String>,
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
    /// Start order (`rest_for_one` restarts the children after the failed one).
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
    /// sha256 of the redirect's delta (4b.4). The content lives in the steer
    /// mail's body, never in this log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_sha: Option<String>,
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
    /// A redirect without a delta, or a delta on another op, or one that is
    /// not a sha256 (4b.4).
    BadSteer,
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
            Self::BadSteer => "bad_steer",
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
    /// `flywheel halt` cancelled the subtree. No relaunch, no new steers.
    Halted,
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
    /// Deaf facts already acted on.
    pub deaf_seen: BTreeSet<String>,
    /// Latest time seen.
    pub now_ms: u64,
    /// Generation floor for crews a failure halted: a re-adopted crew starts
    /// above it, so effect keys (`relaunch:<id>:<gen>`) and the generation
    /// fence never see an old incarnation's values again.
    pub retired_gen: BTreeMap<String, u64>,
    /// In-flight bead assignments, keyed by task id.
    #[serde(default)]
    pub assignments: BTreeMap<String, Assignment>,
    /// Times a task was assigned and then ended abnormally.
    #[serde(default)]
    pub task_abnormal: BTreeMap<String, u32>,
    /// Task ids that have ended abnormally `poison` times. Not reassigned.
    #[serde(default)]
    pub parked_tasks: BTreeSet<String>,
    /// Lead children already adopted. Stops a later round from bumping
    /// `sup_epoch` again for the same death.
    #[serde(default)]
    pub adopted: BTreeSet<String>,
    /// Escalate fact keys already spent as one `ChildEscalated`.
    #[serde(default)]
    pub escalated_facts: BTreeSet<String>,
    /// Children that reported `Proved`. `Done` is not this set.
    #[serde(default)]
    pub proved: BTreeSet<String>,
    /// The integration child (or the operator) said the join holds together.
    #[serde(default)]
    pub integration_proved: bool,
    /// Open conflict keys. Any one keeps the goal unproved.
    #[serde(default)]
    pub conflicts: BTreeSet<String>,
    /// Peer grants, keyed `a` + unit separator + `b`.
    #[serde(default)]
    pub peers: BTreeMap<String, PeerRow>,
    /// Last usage each child reported. A large `usd_micros` does not change status.
    #[serde(default)]
    pub usages: BTreeMap<String, UsageReport>,
    /// Quorum or first-k already sent `Exit` to the children that are not proved.
    #[serde(default)]
    pub cancel_rest_done: bool,
    /// Join met, integration proved, no open conflict, and no live assignment.
    #[serde(default)]
    pub goal_proved: bool,
    /// First tick that a wall budget was in force. `0` means not started.
    #[serde(default)]
    pub wall_origin_ms: u64,
    /// The wall budget already paused and escalated once.
    #[serde(default)]
    pub budget_escalated: bool,
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
            deaf_seen: BTreeSet::new(),
            now_ms: 0,
            retired_gen: BTreeMap::new(),
            assignments: BTreeMap::new(),
            task_abnormal: BTreeMap::new(),
            parked_tasks: BTreeSet::new(),
            adopted: BTreeSet::new(),
            escalated_facts: BTreeSet::new(),
            proved: BTreeSet::new(),
            integration_proved: false,
            conflicts: BTreeSet::new(),
            peers: BTreeMap::new(),
            usages: BTreeMap::new(),
            cancel_rest_done: false,
            goal_proved: false,
            wall_origin_ms: 0,
            budget_escalated: false,
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
    /// A new host process takes over an existing context: it is `me` from now
    /// on (the log's `Hello` names the FIRST host). Logged, so replay agrees.
    Rebind {
        /// The new host's holder id.
        me: String,
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
    /// A child (a lead, in 4b.2) ended abnormally. One restart slot, the
    /// parent's strategy, relaunch at `generation + 1`. Not a free restart:
    /// a full window escalates instead.
    ChildEscalated {
        /// Child.
        child: String,
        /// Why the child escalated. The fold spends a slot either way.
        reason: String,
        /// Time.
        now_ms: u64,
        /// Fact key, when this came from a lead's escalate file. The same
        /// key is one restart. Direct messages omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fact_key: Option<String>,
    },
    /// A new parent takes an orphaned lead. Accepted only when the child is
    /// a lead, its last progress is at least `t_orphan_ms` old, and
    /// `sup_epoch` is newer than the one this host already accepted.
    Adopt {
        /// The lead child.
        child: String,
        /// The parent that is taking it.
        new_parent: String,
        /// Epoch the new parent holds.
        sup_epoch: u64,
        /// Time.
        now_ms: u64,
    },
    /// The inbox host's `deaf` fact for the child (its durable key, so
    /// re-reading the facts file never ends a second incarnation).
    Deaf {
        /// Child.
        child: String,
        /// The fact's key.
        key: String,
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
        /// sha256 of the delta (required for `redirect`, refused for the rest).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delta_sha: Option<String>,
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
    /// Beads ready to hand out. Overlaps stay pending; nothing is assigned
    /// when this supervisor is not running.
    ReadyBeads {
        /// Candidates, in order.
        beads: Vec<ReadyBead>,
        /// Time.
        now_ms: u64,
    },
    /// The ready-bead read failed. Assign nothing.
    ReadyBeadsFailed {
        /// Time.
        now_ms: u64,
    },
    /// Clock.
    Tick {
        /// Time.
        now_ms: u64,
    },
    /// The child proved its own acceptance at `generation`. Not `Done`.
    Proved {
        /// Child.
        child: String,
        /// Gen it reports.
        generation: u64,
        /// Time.
        now_ms: u64,
    },
    /// The integration check. `proved: false` clears a previous pass.
    Integration {
        /// Whether the joined work holds together.
        proved: bool,
        /// Time.
        now_ms: u64,
    },
    /// Open or close one conflict key. An open key blocks the goal.
    Conflict {
        /// Stable key.
        key: String,
        /// `true` opens it, `false` closes it.
        open: bool,
        /// Time.
        now_ms: u64,
    },
    /// Record a peer grant. A stale epoch is rejected. This does not
    /// write a toron row.
    PeerGrant {
        /// One side.
        a: String,
        /// The other side.
        b: String,
        /// Verbs, as written.
        verbs: String,
        /// Expiry. `0` if the caller did not set one.
        until_ms: u64,
        /// Epoch the sender holds.
        sup_epoch: u64,
        /// Time.
        now_ms: u64,
    },
    /// Cancel the subtree and stop. A second halt at the same epoch is a no-op.
    Halt {
        /// Epoch the sender holds.
        sup_epoch: u64,
        /// Time.
        now_ms: u64,
    },
    /// Self-reported usage. Recorded. Never changes `status`.
    Usage {
        /// Child.
        child: String,
        /// Input tokens.
        input_tokens: u64,
        /// Output tokens.
        output_tokens: u64,
        /// Micro-dollars. Advisory.
        usd_micros: u64,
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
        /// Crew relaunch is a pane control; a lead is a nested supervise process.
        /// Old command lines omit this and read as [`ChildKind::Crew`].
        #[serde(default)]
        kind: ChildKind,
    },
    /// Stop the child's pane (cancel, subtree cancel).
    Exit {
        /// Child.
        child: String,
        /// Gen.
        generation: u64,
    },
    /// Append the steer envelope. The doorbell rings from its `priority`;
    /// this command does not type a pane.
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
        /// `op != resume`. The envelope's `priority` is derived from the op,
        /// not from this flag. The host does not ring from it.
        urgent: bool,
        /// sha256 of the redirect's delta (the body of the mail).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delta_sha: Option<String>,
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
    /// Hand a bead to a crew. The host appends `task@1` and does not write a pane.
    Assign {
        /// Task id.
        task: String,
        /// Crew.
        child: String,
        /// The crew's generation.
        generation: u64,
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
            Self::Relaunch {
                child, generation, ..
            } => format!("relaunch:{child}:{generation}"),
            Self::Exit { child, generation } => format!("exit:{child}:{generation}"),
            Self::Assign {
                task,
                child,
                generation,
            } => format!("assign:{task}:{child}:{generation}"),
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

/// A delta sha: 64 lowercase hex digits.
fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The delta a child currently works under: the newest redirect of it that was
/// acked or applied by a restart (the new incarnation reads that mail first).
impl SupModel {
    /// See above.
    #[cfg(test)]
    pub fn current_delta(&self, child: &str) -> Option<&str> {
        self.steers
            .values()
            .rev()
            .find(|s| {
                s.child == child
                    && s.delta_sha.is_some()
                    && matches!(s.state, SteerState::Acked | SteerState::AppliedByRestart)
            })
            .and_then(|s| s.delta_sha.as_deref())
    }
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
        c.state = ChildState::Halted;
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

fn drop_assignments(next: &mut SupModel, id: &str) {
    let gone: Vec<String> = next
        .assignments
        .iter()
        .filter(|(_, a)| a.child == id)
        .map(|(t, _)| t.clone())
        .collect();
    for t in gone {
        next.assignments.remove(&t);
    }
}

fn join_met(m: &SupModel) -> bool {
    let proved_n = m.proved.iter().filter(|id| id.as_str() != PASS).count() as u32;
    match m.policy.join {
        Join::All => {
            let crews: Vec<&str> = m
                .children
                .keys()
                .filter(|id| id.as_str() != PASS)
                .map(String::as_str)
                .collect();
            !crews.is_empty() && crews.iter().all(|id| m.proved.contains(*id))
        }
        Join::Quorum(k) | Join::First(k) => k > 0 && proved_n >= k,
    }
}

/// P12. A join by itself is not the goal: integration, a clear conflict
/// set, and an empty assignment book all have to hold, and only while running.
fn refresh_goal(next: &mut SupModel, cmds: &mut Vec<SupCmd>) {
    let met = join_met(next);
    if met
        && next.policy.join.cancels_rest()
        && !next.cancel_rest_done
        && next.status == Status::Running
    {
        next.cancel_rest_done = true;
        let victims: Vec<(String, u64)> = next
            .children
            .iter()
            .filter(|(id, c)| id.as_str() != PASS && !next.proved.contains(*id) && c.state.live())
            .map(|(id, c)| (id.clone(), c.generation))
            .collect();
        for (id, generation) in victims {
            if let Some(c) = next.children.get_mut(&id) {
                c.state = ChildState::Cancelled;
            }
            drop_assignments(next, &id);
            cmds.push(SupCmd::Exit {
                child: id,
                generation,
            });
        }
    }
    next.goal_proved = next.status == Status::Running
        && met
        && next.integration_proved
        && next.conflicts.is_empty()
        && next.assignments.is_empty();
}

fn epoch_gate(next: &mut SupModel, cmds: &mut Vec<SupCmd>, epoch: u64, child: &str) -> bool {
    if epoch < next.accepted_epoch {
        reject(next, cmds, RejectCause::StaleEpoch, child);
        false
    } else if epoch > next.accepted_epoch {
        reject(next, cmds, RejectCause::UnclaimedEpoch, child);
        false
    } else if next.status != Status::Running {
        reject(next, cmds, RejectCause::NotRunning, child);
        false
    } else {
        true
    }
}

fn pause_child(next: &mut SupModel, cmds: &mut Vec<SupCmd>, child: &str, now: u64) {
    let Some(c) = next.children.get_mut(child) else {
        return;
    };
    if !c.state.live() {
        return;
    }
    c.paused = true;
    let generation = c.generation;
    next.steer_seq += 1;
    let seq = next.steer_seq;
    next.steers.insert(
        seq,
        Steer {
            seq,
            child: child.to_string(),
            op: SteerOp::Pause,
            generation,
            sup_epoch: next.accepted_epoch,
            ack_by_ms: now.saturating_add(next.policy.ack_ms),
            offset: None,
            delta_sha: None,
            state: SteerState::Pending,
        },
    );
    cmds.push(SupCmd::AppendSteer {
        seq,
        child: child.to_string(),
        op: SteerOp::Pause,
        generation,
        sup_epoch: next.accepted_epoch,
        urgent: true,
        delta_sha: None,
    });
}

fn halt_subtree(next: &mut SupModel, cmds: &mut Vec<SupCmd>) {
    next.status = Status::Halted;
    let live: Vec<(String, u64)> = next
        .children
        .iter()
        .filter(|(_, c)| c.state.live())
        .map(|(id, c)| (id.clone(), c.generation))
        .collect();
    for (id, generation) in live {
        if let Some(c) = next.children.get_mut(&id) {
            c.state = ChildState::Cancelled;
        }
        drop_assignments(next, &id);
        if id != PASS {
            cmds.push(SupCmd::Exit {
                child: id,
                generation,
            });
        }
    }
    for s in next.steers.values_mut() {
        if s.state == SteerState::Pending {
            s.state = SteerState::Escalated;
        }
    }
    refresh_goal(next, cmds);
}

/// Bead ids in `text`, in order, no duplicates. `bd-` plus lowercase
/// segments. A trailing hyphen is not part of the id.
pub fn bead_ids(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        let head = b[i].is_ascii_lowercase() && &b[i..i + 3] == b"bd-" && bead_atom(b[i + 3]);
        if !head {
            i += 1;
            continue;
        }
        let start = i;
        i += 3;
        while i < b.len() {
            if bead_atom(b[i]) || (b[i] == b'-' && i + 1 < b.len() && bead_atom(b[i + 1])) {
                i += 1;
            } else {
                break;
            }
        }
        if let Ok(s) = std::str::from_utf8(&b[start..i])
            && !out.iter().any(|e: &String| e == s)
        {
            out.push(s.to_string());
        }
    }
    out
}

fn bead_atom(c: u8) -> bool {
    c.is_ascii_digit() || c.is_ascii_lowercase()
}

/// `br hold` argv when the escalation names a bead. `None` means do not
/// spawn `br`. The first id is `--corr`. Never calls the live tracker.
pub fn bead_hold_argv(evidence: &str, reason: &str) -> Option<Vec<String>> {
    let mut ids = bead_ids(evidence);
    for id in bead_ids(reason) {
        if !ids.iter().any(|e| e == &id) {
            ids.push(id);
        }
    }
    let corr = ids.first()?.clone();
    let mut argv = vec![
        "hold".into(),
        "--kind".into(),
        "captain".into(),
        "--corr".into(),
        corr,
    ];
    argv.extend(ids);
    Some(argv)
}

/// The child abnormal-end path already parks at 3. This only refuses to
/// hand the same task out again once it has ended that many times.
fn note_task_end(next: &mut SupModel, id: &str) {
    let gone: Vec<String> = next
        .assignments
        .iter()
        .filter(|(_, a)| a.child == id)
        .map(|(t, _)| t.clone())
        .collect();
    for t in gone {
        next.assignments.remove(&t);
        let n = next.task_abnormal.entry(t.clone()).or_insert(0);
        *n += 1;
        if *n >= next.policy.poison {
            next.parked_tasks.insert(t);
        }
    }
}

fn child_role(c: &Child) -> &str {
    c.spec.role.as_deref().unwrap_or(&c.spec.id)
}

fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| {
        b.iter()
            .any(|y| toron_core::mailbox::reservations::overlaps(x, y))
    })
}

fn pick_assignee(m: &SupModel, bead: &ReadyBead) -> Option<(String, u64)> {
    if m.status != Status::Running || !bead.deps_proved {
        return None;
    }
    if m.parked_tasks.contains(&bead.id)
        || m.task_abnormal.get(&bead.id).copied().unwrap_or(0) >= m.policy.poison
    {
        return None;
    }
    if m.assignments.contains_key(&bead.id) {
        return None;
    }
    // One task in flight per crew, and never more tasks than `crew_cap`.
    // Live panes are the spawn cap; they are not themselves a full book.
    if m.assignments.len() >= m.policy.crew_cap {
        return None;
    }
    if m.assignments
        .values()
        .any(|a| scopes_overlap(&bead.scope_paths, &a.scope_paths))
    {
        return None;
    }
    let busy: BTreeSet<&str> = m.assignments.values().map(|a| a.child.as_str()).collect();
    m.children
        .values()
        .find(|c| {
            c.spec.kind == ChildKind::Crew
                && c.spec.id != PASS
                && c.state == ChildState::Running
                && !busy.contains(c.spec.id.as_str())
                && bead.role == child_role(c)
        })
        .map(|c| (c.spec.id.clone(), c.generation))
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
    let temporary = {
        let Some(c) = next.children.get(id) else {
            return;
        };
        if !c.state.live() {
            return;
        }
        // Already ended and waiting on herdr: one end is one incarnation.
        if c.state == ChildState::NeedsRestart && count_incarnation {
            return;
        }
        c.spec.restart == Restart::Temporary
    };
    if count_incarnation {
        note_task_end(next, id);
    } else {
        drop_assignments(next, id);
    }
    let Some(c) = next.children.get_mut(id) else {
        return;
    };
    if temporary {
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
    // One failure event is ONE slot in the window, however many children the
    // strategy restarts with it (OTP).
    next.restarts.push(now);
    let set = restart_set(next, id);
    for m in &set {
        if m != id {
            drop_assignments(next, m);
        }
    }
    // Siblings still running are stopped first (the failed child already
    // ended, so it gets no Exit); a temporary sibling is stopped for good.
    for m in &set {
        if m == id {
            continue;
        }
        let Some(c) = next.children.get_mut(m) else {
            continue;
        };
        cmds.push(SupCmd::Exit {
            child: m.clone(),
            generation: c.generation,
        });
        if c.spec.restart == Restart::Temporary {
            c.state = ChildState::Gone;
        }
    }
    let lease = next.policy.lease_ms;
    for m in set {
        let Some(c) = next.children.get_mut(&m) else {
            continue;
        };
        if c.state == ChildState::Gone {
            continue;
        }
        c.generation += 1;
        c.state = ChildState::Restarting;
        c.paused = false;
        c.lease_deadline_ms = now.saturating_add(lease);
        let generation = c.generation;
        let kind = c.spec.kind;
        for s in next.steers.values_mut() {
            if s.child == m && s.state == SteerState::Pending {
                s.state = SteerState::AppliedByRestart;
            }
        }
        cmds.push(SupCmd::Relaunch {
            child: m,
            generation,
            kind,
        });
    }
}

/// The children one failure of `failed` restarts, in start order, `failed`
/// included. A pure function of the model, so replay recomputes it exactly.
/// A sibling is in the set only while `Running`: one already `Restarting` (or
/// waiting on herdr) is covered by the restart it has, never restarted twice.
/// The loop pass is cadence, not a crew: it never joins another child's set
/// and its own failure never cascades.
fn restart_set(m: &SupModel, failed: &str) -> Vec<String> {
    let Some(f) = m.children.get(failed) else {
        return Vec::new();
    };
    let strategy = if failed == PASS {
        Strategy::OneForOne
    } else {
        m.policy.strategy
    };
    let mut set: Vec<(u64, String)> = vec![(f.order, failed.to_string())];
    for (id, c) in &m.children {
        if id == failed || id == PASS || c.state != ChildState::Running {
            continue;
        }
        let in_set = match strategy {
            Strategy::OneForOne => false,
            Strategy::RestForOne => c.order > f.order,
            Strategy::OneForAll => true,
        };
        if in_set {
            set.push((c.order, id.clone()));
        }
    }
    set.sort();
    set.into_iter().map(|(_, id)| id).collect()
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
            SupMsg::Rebind { me } => {
                next.me = me.clone();
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
                    } else if next.status == Status::Failed {
                        // D6: a new host incarnation (its own accepted
                        // claim) starts the intensity window over. The
                        // escalation fact stays (`escalations`, the
                        // `Escalate` command already issued). Crews the
                        // failure halted are dropped so the registry
                        // re-adopts them; an operator's steer cancel stays.
                        next.status = Status::Running;
                        next.restarts.clear();
                        let halted: Vec<(String, u64)> = next
                            .children
                            .iter()
                            .filter(|(_, c)| c.state == ChildState::Halted)
                            .map(|(id, c)| (id.clone(), c.generation))
                            .collect();
                        for (id, g) in halted {
                            next.children.remove(&id);
                            next.retired_gen.insert(id, g);
                        }
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
                            generation: next.retired_gen.remove(&spec.id).map_or(1, |g| g + 1),
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
            SupMsg::Done {
                child,
                generation,
                now_ms,
            } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if accept_report(&mut next, &mut cmds, child, *generation) {
                    if let Some(c) = next.children.get_mut(child) {
                        c.state = ChildState::Done;
                        c.abnormal = 0;
                    }
                    drop_assignments(&mut next, child);
                    refresh_goal(&mut next, &mut cmds);
                }
            }
            SupMsg::Deaf { child, key, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if next.deaf_seen.insert(key.clone())
                    && next
                        .children
                        .get(child)
                        .is_some_and(|c| c.state == ChildState::Running)
                {
                    abnormal_end(&mut next, &mut cmds, child, *now_ms, true);
                }
            }
            SupMsg::Exit { child, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if next
                    .children
                    .get(child)
                    .is_some_and(|c| c.state == ChildState::Running)
                {
                    abnormal_end(&mut next, &mut cmds, child, *now_ms, true);
                }
            }
            SupMsg::ChildEscalated {
                child,
                reason: _,
                now_ms,
                fact_key,
            } => {
                // Same slot as any abnormal end. A full window, including a
                // captain's, escalates through `fail_subtree` instead of
                // relaunching for free. A fact key spends that slot once.
                next.now_ms = next.now_ms.max(*now_ms);
                let seen = fact_key
                    .as_ref()
                    .is_some_and(|k| next.escalated_facts.contains(k));
                if !seen
                    && next
                        .children
                        .get(child)
                        .is_some_and(|c| c.state == ChildState::Running)
                {
                    if let Some(k) = fact_key.clone() {
                        next.escalated_facts.insert(k);
                    }
                    abnormal_end(&mut next, &mut cmds, child, *now_ms, true);
                }
            }
            SupMsg::Adopt {
                child,
                new_parent,
                sup_epoch,
                now_ms,
            } => {
                let orphan = next.children.get(child).is_some_and(|c| {
                    c.spec.kind == ChildKind::Lead
                        && c.state == ChildState::Running
                        && now_ms.saturating_sub(c.last_progress_ms) >= next.policy.t_orphan_ms
                });
                if orphan && *sup_epoch > next.accepted_epoch {
                    next.now_ms = next.now_ms.max(*now_ms);
                    next.accepted_epoch = *sup_epoch;
                    next.leader = new_parent.clone();
                    next.adopted.insert(child.clone());
                }
            }
            SupMsg::Cursor { child, cursor } => {
                let mut bump = false;
                for s in next.steers.values_mut() {
                    if s.child == *child
                        && s.state == SteerState::Pending
                        && s.offset.is_some_and(|o| *cursor >= o)
                    {
                        s.state = SteerState::Acked;
                        // An acked redirect moves the crew to a new generation
                        // (4b.4): the contract changed, so a report for the old
                        // one is stale. Only if the child still is at the
                        // generation the steer was issued at.
                        bump |= s.op == SteerOp::Redirect
                            && next
                                .children
                                .get(child)
                                .is_some_and(|c| c.generation == s.generation);
                    }
                }
                if bump && let Some(c) = next.children.get_mut(child) {
                    c.generation += 1;
                }
            }
            SupMsg::Control {
                op,
                child,
                sup_epoch,
                delta_sha,
                now_ms,
            } => {
                next.now_ms = next.now_ms.max(*now_ms);
                let delta_ok = match (op, delta_sha) {
                    (SteerOp::Redirect, Some(d)) => is_sha(d),
                    (SteerOp::Redirect, None) => false,
                    (_, d) => d.is_none(),
                };
                if *sup_epoch < next.accepted_epoch {
                    reject(&mut next, &mut cmds, RejectCause::StaleEpoch, child);
                } else if *sup_epoch > next.accepted_epoch {
                    reject(&mut next, &mut cmds, RejectCause::UnclaimedEpoch, child);
                } else if next.status != Status::Running || next.leader != next.me {
                    reject(&mut next, &mut cmds, RejectCause::NotRunning, child);
                } else if !next.children.get(child).is_some_and(|c| c.state.live()) {
                    reject(&mut next, &mut cmds, RejectCause::UnknownChild, child);
                } else if !delta_ok {
                    reject(&mut next, &mut cmds, RejectCause::BadSteer, child);
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
                            delta_sha: delta_sha.clone(),
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
                        // A redirect names the generation the crew re-claims at
                        // once it acks: its reports at the old one go stale then.
                        generation: if *op == SteerOp::Redirect {
                            generation + 1
                        } else {
                            generation
                        },
                        sup_epoch: *sup_epoch,
                        urgent: *op != SteerOp::Resume,
                        delta_sha: delta_sha.clone(),
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
            SupMsg::ReadyBeads { beads, now_ms } => {
                if next.status == Status::Running {
                    let mut any = false;
                    for bead in beads {
                        let Some((child, generation)) = pick_assignee(&next, bead) else {
                            continue;
                        };
                        next.assignments.insert(
                            bead.id.clone(),
                            Assignment {
                                task: bead.id.clone(),
                                child: child.clone(),
                                generation,
                                scope_paths: bead.scope_paths.clone(),
                            },
                        );
                        cmds.push(SupCmd::Assign {
                            task: bead.id.clone(),
                            child,
                            generation,
                        });
                        any = true;
                    }
                    if any {
                        next.now_ms = next.now_ms.max(*now_ms);
                    }
                }
            }
            SupMsg::ReadyBeadsFailed { .. } => {}
            SupMsg::Tick { now_ms } => {
                // The clock reading is used, not stored: a tick that decides
                // nothing leaves the model equal, so the host logs nothing.
                let now = next.now_ms.max(*now_ms);
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
                        // An earlier failure in this very tick may have restarted
                        // this child as a sibling (fresh lease): not expired now.
                        if next
                            .children
                            .get(&id)
                            .is_some_and(|c| now > c.lease_deadline_ms)
                        {
                            abnormal_end(&mut next, &mut cmds, &id, now, true);
                        }
                    }
                    // Wall budget. Cost is not here: usage never enters this branch.
                    if let Some(wall) = next.policy.max_wall_ms {
                        if next.wall_origin_ms == 0 {
                            next.wall_origin_ms = now;
                        } else if !next.budget_escalated
                            && now.saturating_sub(next.wall_origin_ms) >= wall
                        {
                            next.budget_escalated = true;
                            let ids: Vec<String> = next
                                .children
                                .iter()
                                .filter(|(id, c)| id.as_str() != PASS && c.state.live())
                                .map(|(id, _)| id.clone())
                                .collect();
                            for id in ids {
                                pause_child(&mut next, &mut cmds, &id, now);
                            }
                            escalate(
                                &mut next,
                                &mut cmds,
                                "budget",
                                format!("wall {wall}ms exhausted"),
                            );
                        }
                    }
                }
            }
            SupMsg::Proved {
                child,
                generation,
                now_ms,
            } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if accept_report(&mut next, &mut cmds, child, *generation) {
                    if child != PASS {
                        next.proved.insert(child.clone());
                    }
                    drop_assignments(&mut next, child);
                    refresh_goal(&mut next, &mut cmds);
                }
            }
            SupMsg::Integration { proved, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if next.status != Status::Running {
                    reject(&mut next, &mut cmds, RejectCause::NotRunning, "");
                } else {
                    next.integration_proved = *proved;
                    refresh_goal(&mut next, &mut cmds);
                }
            }
            SupMsg::Conflict { key, open, now_ms } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if next.status != Status::Running {
                    reject(&mut next, &mut cmds, RejectCause::NotRunning, "");
                } else if key.is_empty() {
                    reject(&mut next, &mut cmds, RejectCause::BadSteer, "");
                } else {
                    if *open {
                        next.conflicts.insert(key.clone());
                    } else {
                        next.conflicts.remove(key);
                    }
                    refresh_goal(&mut next, &mut cmds);
                }
            }
            SupMsg::PeerGrant {
                a,
                b,
                verbs,
                until_ms,
                sup_epoch,
                now_ms,
            } => {
                next.now_ms = next.now_ms.max(*now_ms);
                if epoch_gate(&mut next, &mut cmds, *sup_epoch, a) {
                    let key = format!("{a}\u{1f}{b}");
                    next.peers.insert(
                        key,
                        PeerRow {
                            a: a.clone(),
                            b: b.clone(),
                            verbs: verbs.clone(),
                            until_ms: *until_ms,
                        },
                    );
                }
            }
            SupMsg::Halt { sup_epoch, now_ms } => {
                // A second halt at the same epoch changes nothing, so the
                // host does not log it again.
                if next.status == Status::Halted && *sup_epoch == next.accepted_epoch {
                } else if *sup_epoch < next.accepted_epoch {
                    next.now_ms = next.now_ms.max(*now_ms);
                    reject(&mut next, &mut cmds, RejectCause::StaleEpoch, "");
                } else if *sup_epoch > next.accepted_epoch {
                    next.now_ms = next.now_ms.max(*now_ms);
                    reject(&mut next, &mut cmds, RejectCause::UnclaimedEpoch, "");
                } else if next.status != Status::Running {
                    next.now_ms = next.now_ms.max(*now_ms);
                    reject(&mut next, &mut cmds, RejectCause::NotRunning, "");
                } else {
                    next.now_ms = next.now_ms.max(*now_ms);
                    halt_subtree(&mut next, &mut cmds);
                }
            }
            SupMsg::Usage {
                child,
                input_tokens,
                output_tokens,
                usd_micros,
                now_ms,
            } => {
                next.now_ms = next.now_ms.max(*now_ms);
                next.usages.insert(
                    child.clone(),
                    UsageReport {
                        input_tokens: *input_tokens,
                        output_tokens: *output_tokens,
                        usd_micros: *usd_micros,
                    },
                );
            }
            SupMsg::CmdFailed { key, why: _ } => {
                next.failed_cmds += 1;
                // A relaunch that did not land is another failed incarnation
                // of the same child: it spends the budget, never loops free.
                if let Some(rest) = key.strip_prefix("relaunch:")
                    && let Some((id, generation)) = rest.rsplit_once(':')
                    && let Ok(generation) = generation.parse::<u64>()
                    && next.children.get(id).is_some_and(|c| {
                        c.state == ChildState::Restarting && c.generation == generation
                    })
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

    /// A well-formed delta sha for tests that only need one.
    const D: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ME: &str = "lead-1";
    const MIN: u64 = 60_000;

    fn spec(id: &str) -> ChildSpec {
        ChildSpec {
            id: id.into(),
            npub: Some(format!("npub-{id}")),
            restart: Restart::Transient,
            kind: ChildKind::Crew,
            role: None,
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
                SupCmd::Relaunch {
                    child, generation, ..
                } => Some((child.clone(), *generation)),
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

    // ---- 4b.1: strategies (`rest_for_one`, `one_for_all`)

    /// A lead with crews `ids` (start order = list order) under `strategy`.
    fn strat(strategy: Strategy, ids: &[&str]) -> SupModel {
        let mut policy = Policy::for_tier(Tier::Lead);
        policy.strategy = strategy;
        let mut m = SupModel::default();
        let mut msgs = vec![
            SupMsg::Hello {
                me: ME.into(),
                tier: Tier::Lead,
                policy: Some(policy),
            },
            SupMsg::Claim {
                holder: ME.into(),
                sup_epoch: 2,
            },
        ];
        for id in ids {
            msgs.push(SupMsg::Spawn {
                spec: spec(id),
                now_ms: 0,
            });
        }
        for msg in &msgs {
            m = step(&m, msg).0;
        }
        m
    }

    /// Confirm every `Restarting` child's relaunch landed.
    fn settle(m: SupModel) -> SupModel {
        let mut m = m;
        let ids: Vec<(String, u64)> = m
            .children
            .iter()
            .filter(|(_, c)| c.state == ChildState::Restarting)
            .map(|(id, c)| (id.clone(), c.generation))
            .collect();
        for (child, generation) in ids {
            m = step(&m, &SupMsg::Relaunched { child, generation }).0;
        }
        m
    }

    fn exits(c: &[SupCmd]) -> Vec<(String, u64)> {
        c.iter()
            .filter_map(|c| match c {
                SupCmd::Exit { child, generation } => Some((child.clone(), *generation)),
                _ => None,
            })
            .collect()
    }

    fn rl(v: &[(&str, u64)]) -> Vec<(String, u64)> {
        v.iter().map(|(a, g)| (a.to_string(), *g)).collect()
    }

    #[test]
    fn one_for_one_restarts_only_the_failed_child() {
        let (m, c) = step(
            &strat(Strategy::OneForOne, &["a", "b", "c"]),
            &exit("b", 10),
        );
        assert_eq!(relaunches(&c), rl(&[("b", 2)]));
        assert!(exits(&c).is_empty());
        assert_eq!(m.children["c"].generation, 1);
    }

    #[test]
    fn rest_for_one_restarts_the_failed_child_and_those_started_after_it_never_before() {
        let (m, c) = step(
            &strat(Strategy::RestForOne, &["a", "b", "c"]),
            &exit("b", 10),
        );
        assert_eq!(relaunches(&c), rl(&[("b", 2), ("c", 2)]));
        assert_eq!(
            exits(&c),
            rl(&[("c", 1)]),
            "the running sibling is stopped first; the failed child already ended"
        );
        assert_eq!(
            m.children["a"].generation, 1,
            "an earlier child is never touched"
        );
        assert_eq!(m.children["a"].state, ChildState::Running);
        assert_eq!(m.children["c"].state, ChildState::Restarting);
    }

    #[test]
    fn rest_for_one_on_the_last_child_is_one_for_one() {
        let (_, c) = step(
            &strat(Strategy::RestForOne, &["a", "b", "c"]),
            &exit("c", 10),
        );
        assert_eq!(relaunches(&c), rl(&[("c", 2)]));
        assert!(exits(&c).is_empty());
    }

    #[test]
    fn one_for_all_restarts_every_live_child() {
        let (_, c) = step(
            &strat(Strategy::OneForAll, &["a", "b", "c"]),
            &exit("b", 10),
        );
        assert_eq!(relaunches(&c), rl(&[("a", 2), ("b", 2), ("c", 2)]));
        assert_eq!(exits(&c), rl(&[("a", 1), ("c", 1)]));
    }

    #[test]
    fn exits_precede_relaunches_in_the_command_list() {
        let (_, c) = step(
            &strat(Strategy::OneForAll, &["a", "b", "c"]),
            &exit("b", 10),
        );
        let last_exit = c
            .iter()
            .rposition(|x| matches!(x, SupCmd::Exit { .. }))
            .unwrap();
        let first_rl = c
            .iter()
            .position(|x| matches!(x, SupCmd::Relaunch { .. }))
            .unwrap();
        assert!(last_exit < first_rl);
    }

    #[test]
    fn a_cascade_costs_one_intensity_slot_and_the_fourth_event_escalates() {
        let mut m = strat(Strategy::OneForAll, &["a", "b", "c"]);
        for (i, t) in [10u64, 20, 30].into_iter().enumerate() {
            // A different child fails each time, so none reaches the poison ceiling.
            let (n, c) = step(&m, &exit(["a", "b", "c"][i], t));
            assert_eq!(relaunches(&c).len(), 3, "event {i} restarts all three");
            assert_eq!(
                n.restarts.len(),
                i + 1,
                "one slot per failure event, not per child"
            );
            assert_eq!(escalations(&c), 0);
            m = settle(n);
        }
        let (m, c) = step(&m, &exit("a", 40));
        assert_eq!(escalations(&c), 1, "MaxR+1 failure events escalate");
        assert_eq!(m.status, Status::Failed);
        assert!(relaunches(&c).is_empty());
    }

    #[test]
    fn a_restarted_child_keeps_its_place() {
        let m = strat(Strategy::RestForOne, &["a", "b", "c"]);
        let before: Vec<u64> = ["a", "b", "c"]
            .iter()
            .map(|i| m.children[*i].order)
            .collect();
        let (m, _) = step(&m, &exit("a", 10));
        let m = settle(m);
        let after: Vec<u64> = ["a", "b", "c"]
            .iter()
            .map(|i| m.children[*i].order)
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn a_failure_while_a_sibling_is_restarting_does_not_restart_it_twice() {
        let (m, c) = step(
            &strat(Strategy::RestForOne, &["a", "b", "c"]),
            &exit("c", 10),
        );
        assert_eq!(relaunches(&c), rl(&[("c", 2)]));
        // c's relaunch is unconfirmed; b fails: c is covered by that restart.
        let (m, c) = step(&m, &exit("b", 20));
        assert_eq!(
            relaunches(&c),
            rl(&[("b", 2)]),
            "c is Restarting already: not restarted again"
        );
        assert!(exits(&c).is_empty());
        assert_eq!(m.children["c"].generation, 2);
    }

    #[test]
    fn two_leases_expiring_in_one_tick_are_one_cascade_not_two() {
        let m = strat(Strategy::OneForAll, &["a", "b"]);
        let (m, c) = step(
            &m,
            &SupMsg::Tick {
                now_ms: DEFAULT_LEASE_MS + 1,
            },
        );
        assert_eq!(
            relaunches(&c),
            rl(&[("a", 2), ("b", 2)]),
            "b was restarted with a: its lease is fresh"
        );
        assert_eq!(m.restarts.len(), 1);
    }

    #[test]
    fn a_pending_steer_on_a_restarted_sibling_is_applied_by_restart() {
        let m = strat(Strategy::OneForAll, &["a", "b"]);
        let (m, _) = step(
            &m,
            &SupMsg::Control {
                op: SteerOp::Redirect,
                child: "a".into(),
                sup_epoch: 2,
                delta_sha: Some(D.into()),
                now_ms: 1_000,
            },
        );
        let (m, _) = step(&m, &SupMsg::SteerAppended { seq: 1, offset: 7 });
        assert_eq!(m.steers[&1].state, SteerState::Pending);
        let (m, _) = step(&m, &exit("b", 2_000));
        assert_eq!(
            m.steers[&1].state,
            SteerState::AppliedByRestart,
            "never left gating a dead incarnation"
        );
        assert_eq!(m.children["a"].generation, 2);
    }

    #[test]
    fn a_temporary_sibling_is_stopped_not_restarted() {
        let mut m = strat(Strategy::OneForAll, &["a", "b"]);
        m.children.get_mut("a").unwrap().spec.restart = Restart::Temporary;
        let (m, c) = step(&m, &exit("b", 10));
        assert_eq!(relaunches(&c), rl(&[("b", 2)]));
        assert_eq!(exits(&c), rl(&[("a", 1)]));
        assert_eq!(m.children["a"].state, ChildState::Gone);
    }

    #[test]
    fn finished_and_parked_siblings_are_not_in_the_set() {
        let mut m = strat(Strategy::OneForAll, &["a", "b", "c"]);
        m.children.get_mut("a").unwrap().state = ChildState::Done;
        m.children.get_mut("c").unwrap().state = ChildState::Parked;
        let (_, c) = step(&m, &exit("b", 10));
        assert_eq!(relaunches(&c), rl(&[("b", 2)]));
    }

    #[test]
    fn the_loop_pass_child_is_never_a_sibling_and_never_cascades() {
        let m = strat(Strategy::OneForAll, &["a", PASS]);
        let (_, c) = step(&m, &exit("a", 10));
        assert_eq!(
            relaunches(&c),
            rl(&[("a", 2)]),
            "the pass is cadence, not a crew"
        );
        let (_, c) = step(&m, &exit(PASS, 10));
        assert_eq!(
            relaunches(&c),
            rl(&[(PASS, 2)]),
            "a pass failure does not restart crews"
        );
    }

    #[test]
    fn a_parked_failing_child_does_not_cascade() {
        let mut m = strat(Strategy::OneForAll, &["a", "b"]);
        m.children.get_mut("b").unwrap().abnormal = 2;
        let (m, c) = step(&m, &exit("b", 10));
        assert!(relaunches(&c).is_empty());
        assert_eq!(m.children["b"].state, ChildState::Parked);
        assert_eq!(m.children["a"].state, ChildState::Running);
    }

    #[test]
    fn a_cascade_deferred_by_herdr_down_runs_once_when_it_is_back() {
        let m = strat(Strategy::OneForAll, &["a", "b"]);
        let (m, _) = step(&m, &SupMsg::HerdrDown { now_ms: 5 });
        let (m, c) = step(&m, &exit("a", 10));
        assert!(relaunches(&c).is_empty());
        let (m, _) = step(&m, &SupMsg::HerdrUp { now_ms: 20 });
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: 21 });
        assert_eq!(relaunches(&c), rl(&[("a", 2), ("b", 2)]));
        assert_eq!(m.restarts.len(), 1);
        let (_, c) = step(&m, &SupMsg::Tick { now_ms: 22 });
        assert!(relaunches(&c).is_empty(), "exactly once");
    }

    #[test]
    fn strategy_round_trips_and_parses_the_fixture_words() {
        for (w, s) in [
            ("one_for_one", Strategy::OneForOne),
            ("rest_for_one", Strategy::RestForOne),
            ("one_for_all", Strategy::OneForAll),
        ] {
            assert_eq!(Strategy::parse(w), Some(s));
            assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{w}\""));
        }
        assert_eq!(Strategy::parse("nope"), None);
    }

    /// Seeded generator: no proptest in this crate, so a plain LCG walks many
    /// message sequences and checks the invariants after each step.
    #[test]
    fn invariants_hold_over_generated_sequences_for_every_strategy() {
        for strategy in [
            Strategy::OneForOne,
            Strategy::RestForOne,
            Strategy::OneForAll,
        ] {
            for seed in 0..200u64 {
                let mut x = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let mut next = move || {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (x >> 33) as usize
                };
                let ids = ["a", "b", "c", "d"];
                let mut m = strat(strategy, &ids);
                let mut msgs: Vec<SupMsg> = Vec::new();
                let mut keys = std::collections::BTreeSet::new();
                let mut t = 0u64;
                for _ in 0..40 {
                    t += 1 + (next() % 4) as u64 * 60_000;
                    let id = ids[next() % ids.len()];
                    let msg = match next() % 4 {
                        0 | 1 => exit(id, t),
                        2 => SupMsg::Tick { now_ms: t },
                        _ => SupMsg::Relaunched {
                            child: id.into(),
                            generation: m.children[id].generation,
                        },
                    };
                    let (n, c) = step(&m, &msg);
                    // (child, generation) of a relaunch is never reused.
                    for (ch, g) in relaunches(&c) {
                        assert!(
                            keys.insert((ch.clone(), g)),
                            "{strategy:?} seed {seed}: relaunch:{ch}:{g} reused"
                        );
                        assert_eq!(g, n.children[&ch].generation);
                    }
                    for (id, ch) in &n.children {
                        assert!(
                            ch.generation >= m.children[id].generation,
                            "generation never goes back"
                        );
                        assert_eq!(ch.order, m.children[id].order, "order is stable");
                    }
                    assert!(
                        n.restarts.len() as u32 <= n.policy.max_r,
                        "the window never exceeds MaxR"
                    );
                    msgs.push(msg);
                    m = n;
                }
                // P8: replay gives the same model.
                let mut all = vec![
                    SupMsg::Hello {
                        me: ME.into(),
                        tier: Tier::Lead,
                        policy: Some({
                            let mut p = Policy::for_tier(Tier::Lead);
                            p.strategy = strategy;
                            p
                        }),
                    },
                    SupMsg::Claim {
                        holder: ME.into(),
                        sup_epoch: 2,
                    },
                ];
                for id in ids {
                    all.push(SupMsg::Spawn {
                        spec: spec(id),
                        now_ms: 0,
                    });
                }
                all.extend(msgs);
                assert_eq!(
                    replay(&all).0,
                    m,
                    "{strategy:?} seed {seed}: replay differs"
                );
            }
        }
    }

    // ---- 4b.4: the redirect delta (interim form)

    fn redirect(child: &str, sha: Option<&str>) -> SupMsg {
        SupMsg::Control {
            op: SteerOp::Redirect,
            child: child.into(),
            sup_epoch: 2,
            delta_sha: sha.map(Into::into),
            now_ms: 1_000,
        }
    }

    const D2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_redirect_without_a_delta_is_rejected_bad_steer_and_nothing_is_sent() {
        let (m, c) = step(&base(), &redirect("a", None));
        assert_eq!(rejects(&c), vec![RejectCause::BadSteer]);
        assert!(m.steers.is_empty());
        assert!(!c.iter().any(|x| matches!(x, SupCmd::AppendSteer { .. })));
    }

    #[test]
    fn a_delta_that_is_not_a_sha_or_that_rides_another_op_is_rejected() {
        for bad in [
            "",
            "not-a-sha",
            &"A".repeat(64),
            &"a".repeat(63),
            &"g".repeat(64),
            "../../x",
        ] {
            let (_, c) = step(&base(), &redirect("a", Some(bad)));
            assert_eq!(rejects(&c), vec![RejectCause::BadSteer], "{bad:?}");
        }
        for op in [SteerOp::Pause, SteerOp::Resume, SteerOp::Cancel] {
            let (_, c) = step(
                &base(),
                &SupMsg::Control {
                    op,
                    child: "a".into(),
                    sup_epoch: 2,
                    delta_sha: Some(D.into()),
                    now_ms: 1,
                },
            );
            assert_eq!(
                rejects(&c),
                vec![RejectCause::BadSteer],
                "{op:?} takes no delta"
            );
        }
    }

    #[test]
    fn a_redirect_with_a_delta_records_the_sha_and_names_the_generation_to_reclaim_at() {
        let (m, c) = step(&base(), &redirect("a", Some(D)));
        assert_eq!(m.steers[&1].delta_sha.as_deref(), Some(D));
        assert_eq!(
            m.children["a"].generation, 1,
            "the crew has not acked: still g"
        );
        match c.as_slice() {
            [
                SupCmd::AppendSteer {
                    generation,
                    delta_sha,
                    ..
                },
            ] => {
                assert_eq!(*generation, 2, "the mail names g+1");
                assert_eq!(delta_sha.as_deref(), Some(D));
            }
            other => panic!("{other:?}"),
        }
    }

    fn acked_redirect() -> SupModel {
        let (m, _) = step(&base(), &redirect("a", Some(D)));
        let (m, _) = step(&m, &SupMsg::SteerAppended { seq: 1, offset: 7 });
        step(
            &m,
            &SupMsg::Cursor {
                child: "a".into(),
                cursor: 7,
            },
        )
        .0
    }

    #[test]
    fn an_acked_redirect_bumps_the_generation_and_the_old_one_goes_stale() {
        let m = acked_redirect();
        assert_eq!(m.steers[&1].state, SteerState::Acked);
        assert_eq!(m.children["a"].generation, 2);
        assert_eq!(m.current_delta("a"), Some(D));
        let (_, c) = step(
            &m,
            &SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: 2_000,
            },
        );
        assert_eq!(
            rejects(&c),
            vec![RejectCause::StaleGen],
            "a late done(g) is stale"
        );
        let (m2, c) = step(
            &m,
            &SupMsg::Progress {
                child: "a".into(),
                generation: 2,
                now_ms: 2_000,
            },
        );
        assert!(rejects(&c).is_empty(), "the crew re-claimed at g+1");
        assert_eq!(m2.children["a"].last_progress_ms, 2_000);
    }

    #[test]
    fn re_reading_the_cursor_does_not_bump_twice() {
        let m = acked_redirect();
        let (m, _) = step(
            &m,
            &SupMsg::Cursor {
                child: "a".into(),
                cursor: 99,
            },
        );
        assert_eq!(m.children["a"].generation, 2);
    }

    #[test]
    fn an_acked_pause_does_not_bump_the_generation() {
        let (m, _) = step(
            &base(),
            &SupMsg::Control {
                op: SteerOp::Pause,
                child: "a".into(),
                sup_epoch: 2,
                delta_sha: None,
                now_ms: 1,
            },
        );
        let (m, _) = step(&m, &SupMsg::SteerAppended { seq: 1, offset: 7 });
        let (m, _) = step(
            &m,
            &SupMsg::Cursor {
                child: "a".into(),
                cursor: 7,
            },
        );
        assert_eq!(m.children["a"].generation, 1);
    }

    #[test]
    fn a_redirect_the_crew_never_acks_is_applied_by_a_restart_at_g_plus_one_carrying_the_delta() {
        let (m, _) = step(&base(), &redirect("a", Some(D)));
        let (m, _) = step(&m, &SupMsg::SteerAppended { seq: 1, offset: 7 });
        let (m, c) = step(
            &m,
            &SupMsg::Tick {
                now_ms: 1_000 + DEFAULT_ACK_MS + 1,
            },
        );
        assert_eq!(relaunches(&c), vec![("a".to_string(), 2)], "g+1, not g+2");
        assert_eq!(m.steers[&1].state, SteerState::AppliedByRestart);
        assert_eq!(
            m.current_delta("a"),
            Some(D),
            "the new incarnation reads that mail first"
        );
        let (_, c) = step(
            &m,
            &SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: 9,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleGen]);
    }

    #[test]
    fn the_newest_acked_or_restart_applied_delta_is_the_current_one() {
        let m = acked_redirect();
        let (m, _) = step(
            &m,
            &SupMsg::Progress {
                child: "a".into(),
                generation: 2,
                now_ms: 1_500,
            },
        );
        let (m, _) = step(&m, &redirect("a", Some(D2)));
        assert_eq!(
            m.current_delta("a"),
            Some(D),
            "a pending one is not current yet"
        );
        let (m, _) = step(&m, &SupMsg::SteerAppended { seq: 2, offset: 9 });
        let (m, _) = step(
            &m,
            &SupMsg::Cursor {
                child: "a".into(),
                cursor: 9,
            },
        );
        assert_eq!(m.current_delta("a"), Some(D2));
        assert_eq!(m.children["a"].generation, 3);
        assert_eq!(m.current_delta("b"), None);
    }

    #[test]
    fn a_superseded_redirect_is_not_the_current_delta() {
        let (m, _) = step(&base(), &redirect("a", Some(D)));
        let (m, _) = step(&m, &redirect("a", Some(D2)));
        assert_eq!(m.steers[&1].state, SteerState::Superseded);
        assert_eq!(m.current_delta("a"), None);
    }

    #[test]
    fn the_log_holds_the_sha_and_never_the_delta_text() {
        let (m, c) = step(&base(), &redirect("a", Some(D)));
        let json = serde_json::to_string(&(m, c)).unwrap();
        assert!(json.contains(D));
        assert!(
            !json.contains("goal"),
            "no delta content in the model or the commands"
        );
    }

    #[test]
    fn an_old_log_line_without_the_field_still_parses() {
        let line = r#"{"kind":"control","op":"cancel","child":"a","sup_epoch":2,"now_ms":5}"#;
        let m: SupMsg = serde_json::from_str(line).unwrap();
        assert!(matches!(
            m,
            SupMsg::Control {
                delta_sha: None,
                ..
            }
        ));
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
        assert!(
            m.children.values().all(|c| !c.state.live()),
            "subtree cancelled"
        );
    }

    fn failed() -> SupModel {
        let mut m = base();
        for (i, id) in ["a", "b", "a"].into_iter().enumerate() {
            m = crash_and_relaunch(m, id, 1_000 + i as u64 * 1_000).0;
        }
        let (m, c) = step(&m, &exit("b", 5_000));
        assert_eq!(escalations(&c), 1);
        assert_eq!(m.status, Status::Failed);
        m
    }

    fn claim(holder: &str, e: u64) -> SupMsg {
        SupMsg::Claim {
            holder: holder.into(),
            sup_epoch: e,
        }
    }

    #[test]
    fn an_own_claim_after_failure_is_a_fresh_incarnation_keeping_the_escalation() {
        let m = failed();
        let (r, c) = step(&m, &claim(ME, 3));
        assert_eq!(r.status, Status::Running);
        assert!(c.is_empty(), "the reset itself asks for nothing");
        assert!(r.restarts.is_empty());
        assert_eq!(r.escalations, 1, "the escalation fact is kept");
        assert!(
            r.children.is_empty(),
            "halted children are re-adopted from the registry"
        );
        // Re-adopted, it supervises again from a clean budget, in a NEW
        // generation: the old incarnation's effect keys (`relaunch:a:3`) are
        // settled in the journal and must not swallow the new one's, and a
        // late report from the old generation stays fenced.
        let old = (m.children["a"].generation, m.children["b"].generation);
        let r = step(
            &r,
            &SupMsg::Spawn {
                spec: spec("a"),
                now_ms: 9_000,
            },
        )
        .0;
        let r = step(
            &r,
            &SupMsg::Spawn {
                spec: spec("b"),
                now_ms: 9_000,
            },
        )
        .0;
        assert_eq!(r.children["a"].generation, old.0 + 1);
        assert_eq!(r.children["b"].generation, old.1 + 1);
        assert_eq!(r.children["a"].abnormal, 0);
        let (late, c) = step(
            &r,
            &SupMsg::Done {
                child: "a".into(),
                generation: old.0,
                now_ms: 9_050,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleGen]);
        assert_eq!(late.children["a"].state, ChildState::Running);
        let (r, c) = step(&r, &exit("a", 9_100));
        let rl = relaunches(&c);
        assert_eq!(rl.len(), 1);
        assert!(
            rl[0].1 > old.0 + 1,
            "a relaunch key never reuses a settled one: {rl:?} vs {old:?}"
        );
        assert_eq!(r.status, Status::Running);
    }

    #[test]
    fn only_the_hosts_own_accepted_claim_resets_a_failure() {
        let m = failed();
        let (s, _) = step(&m, &claim(ME, 2));
        assert_eq!(
            s.status,
            Status::Failed,
            "a rejected (stale) own claim resets nothing"
        );
        let (s, _) = step(&m, &claim("other", 3));
        assert_eq!(
            s.status,
            Status::Superseded,
            "a foreign claim supersedes, it does not reset"
        );
    }

    #[test]
    fn an_operator_cancel_survives_a_new_incarnation() {
        let mut m = failed();
        let mut gone = m.children["a"].clone();
        gone.state = ChildState::Cancelled;
        m.children.insert("c".into(), gone);
        let (r, _) = step(&m, &claim(ME, 3));
        assert_eq!(
            r.children["c"].state,
            ChildState::Cancelled,
            "a steer cancel is final"
        );
        assert!(
            !r.children.contains_key("a"),
            "a subtree-halted crew is re-adoptable"
        );
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
        m = step(
            &m,
            &SupMsg::Claim {
                holder: ME.into(),
                sup_epoch: 1,
            },
        )
        .0;
        for id in ["a", "b", "c"] {
            m = step(
                &m,
                &SupMsg::Spawn {
                    spec: spec(id),
                    now_ms: 0,
                },
            )
            .0;
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
        m = step(
            &m,
            &SupMsg::Spawn {
                spec: ChildSpec {
                    id: PASS.into(),
                    npub: None,
                    restart: Restart::Permanent,
                    kind: ChildKind::Crew,
                    role: None,
                },
                now_ms: 0,
            },
        )
        .0;
        let mut esc = 0;
        for t in 1..=6u64 {
            let (n, c) = step(
                &m,
                &SupMsg::PassExit {
                    ok: false,
                    now_ms: t * 1_000,
                },
            );
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
        let (_, c) = step(
            &m,
            &SupMsg::Tick {
                now_ms: DEFAULT_LEASE_MS + 10,
            },
        );
        assert_eq!(
            relaunches(&c),
            vec![("b".to_string(), 2)],
            "a was renewed, b was not"
        );
    }

    // ---- P9: sup_epoch fence

    #[test]
    fn a_control_below_the_accepted_epoch_is_rejected() {
        let m = base();
        let ctl = |e| SupMsg::Control {
            op: SteerOp::Cancel,
            child: "a".into(),
            sup_epoch: e,
            delta_sha: None,
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
        let (m, c) = run(
            m,
            &[
                exit("a", 1),
                SupMsg::Tick {
                    now_ms: DEFAULT_LEASE_MS * 2,
                },
            ],
        );
        assert!(
            relaunches(&c).is_empty(),
            "a superseded supervisor restarts nothing"
        );
        assert_eq!(m.children["a"].state, ChildState::Running);
        // And its own old epoch is now stale.
        let (_, c) = step(
            &m,
            &SupMsg::Control {
                op: SteerOp::Cancel,
                child: "a".into(),
                sup_epoch: 2,
                delta_sha: None,
                now_ms: 5,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleEpoch]);
    }

    #[test]
    fn a_restarted_host_rebinds_and_its_own_claim_is_not_a_supersession() {
        let mut m = SupModel::new("host-1", Tier::Lead);
        m = SupervisorProgram::update(
            &m,
            &SupMsg::Claim {
                holder: "host-1".into(),
                sup_epoch: 1,
            },
        )
        .model;
        // host-2 starts over the same log: without Rebind its claim reads as foreign.
        let foreign = SupervisorProgram::update(
            &m,
            &SupMsg::Claim {
                holder: "host-2".into(),
                sup_epoch: 2,
            },
        )
        .model;
        assert_eq!(foreign.status, Status::Superseded);
        let m = SupervisorProgram::update(
            &m,
            &SupMsg::Rebind {
                me: "host-2".into(),
            },
        )
        .model;
        let m = SupervisorProgram::update(
            &m,
            &SupMsg::Claim {
                holder: "host-2".into(),
                sup_epoch: 2,
            },
        )
        .model;
        assert_eq!(
            (m.status, m.leader.as_str(), m.accepted_epoch),
            (Status::Running, "host-2", 2)
        );
        // and the old host, replaying the same log, is the one superseded
        let old = SupervisorProgram::update(
            &SupModel::new("host-1", Tier::Lead),
            &SupMsg::Claim {
                holder: "host-2".into(),
                sup_epoch: 2,
            },
        )
        .model;
        assert_eq!(old.status, Status::Superseded);
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
                delta_sha: Some(D.into()),
                now_ms: 1_000,
            },
        );
        assert!(matches!(
            c.as_slice(),
            [SupCmd::AppendSteer {
                seq: 1,
                urgent: true,
                generation: 2,
                ..
            }]
        ));
        step(&m, &SupMsg::SteerAppended { seq: 1, offset: 7 }).0
    }

    #[test]
    fn while_a_steer_is_unacked_no_progress_or_done_is_accepted() {
        let m = steered();
        for msg in [
            SupMsg::Progress {
                child: "a".into(),
                generation: 1,
                now_ms: 2_000,
            },
            SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: 2_000,
            },
        ] {
            let (m2, c) = step(&m, &msg);
            assert_eq!(rejects(&c), vec![RejectCause::SteerPending]);
            assert_eq!(m2.children["a"].state, ChildState::Running);
        }
        // The other child is not gated.
        let (_, c) = step(
            &m,
            &SupMsg::Progress {
                child: "b".into(),
                generation: 1,
                now_ms: 2_000,
            },
        );
        assert!(rejects(&c).is_empty());
    }

    #[test]
    fn the_cursor_reaching_the_steer_offset_acks_it_and_opens_the_gate() {
        let m = steered();
        let (m, _) = step(
            &m,
            &SupMsg::Cursor {
                child: "a".into(),
                cursor: 6,
            },
        );
        assert_eq!(m.steers[&1].state, SteerState::Pending, "6 < 7");
        let (m, _) = step(
            &m,
            &SupMsg::Cursor {
                child: "a".into(),
                cursor: 7,
            },
        );
        assert_eq!(m.steers[&1].state, SteerState::Acked);
        // The steer here is a redirect: the crew re-claims at g+1.
        let (_, c) = step(
            &m,
            &SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: 3_000,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleGen]);
        let (_, c) = step(
            &m,
            &SupMsg::Done {
                child: "a".into(),
                generation: 2,
                now_ms: 3_000,
            },
        );
        assert!(rejects(&c).is_empty());
    }

    #[test]
    fn a_cursor_of_another_child_does_not_ack() {
        let m = steered();
        let (m, _) = step(
            &m,
            &SupMsg::Cursor {
                child: "b".into(),
                cursor: 99,
            },
        );
        assert_eq!(m.steers[&1].state, SteerState::Pending);
    }

    #[test]
    fn a_crew_ignoring_a_steer_is_restarted_at_the_next_gen_and_its_late_done_is_fenced() {
        let m = steered();
        let ack_by = m.steers[&1].ack_by_ms;
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: ack_by });
        assert!(
            relaunches(&c).is_empty(),
            "at ack_by the crew still has time"
        );
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: ack_by + 1 });
        assert_eq!(relaunches(&c), vec![("a".to_string(), 2)]);
        assert_eq!(m.steers[&1].state, SteerState::AppliedByRestart);
        let (_, c) = step(
            &m,
            &SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: ack_by + 2,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::StaleGen]);
    }

    #[test]
    fn an_ignored_cancel_ends_the_child_instead_of_restarting_it() {
        let (m, _) = step(
            &base(),
            &SupMsg::Control {
                op: SteerOp::Cancel,
                child: "a".into(),
                sup_epoch: 2,
                delta_sha: None,
                now_ms: 0,
            },
        );
        let (m, c) = step(
            &m,
            &SupMsg::Tick {
                now_ms: DEFAULT_ACK_MS + 1,
            },
        );
        assert!(relaunches(&c).iter().all(|(id, _)| id != "a"));
        assert!(c.contains(&SupCmd::Exit {
            child: "a".into(),
            generation: 1
        }));
        assert_eq!(m.children["a"].state, ChildState::Cancelled);
        assert_eq!(m.steers[&1].state, SteerState::AppliedByRestart);
    }

    #[test]
    fn a_newer_steer_supersedes_the_pending_one() {
        let m = steered();
        let (m, _) = step(
            &m,
            &SupMsg::Control {
                op: SteerOp::Pause,
                child: "a".into(),
                sup_epoch: 2,
                delta_sha: None,
                now_ms: 2_000,
            },
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
        let (m, c) = step(
            &m,
            &SupMsg::Tick {
                now_ms: m.steers[&1].ack_by_ms + 1,
            },
        );
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
            parks += c
                .iter()
                .filter(|c| matches!(c, SupCmd::Park { .. }))
                .count();
        }
        assert_eq!(parks, 1);
        assert_eq!(m.children["a"].state, ChildState::Parked);
    }

    #[test]
    fn a_normal_end_is_final_and_a_temporary_child_is_never_restarted() {
        let (m, _) = step(
            &base(),
            &SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: 5,
            },
        );
        let (m, c) = step(&m, &exit("a", 6));
        assert!(relaunches(&c).is_empty(), "Done is final");
        assert_eq!(m.children["a"].state, ChildState::Done);
        let mut m = base();
        m = step(
            &m,
            &SupMsg::Spawn {
                spec: ChildSpec {
                    id: "t".into(),
                    npub: None,
                    restart: Restart::Temporary,
                    kind: ChildKind::Crew,
                    role: None,
                },
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
            m = step(
                &m,
                &SupMsg::Spawn {
                    spec: spec(id),
                    now_ms: 0,
                },
            )
            .0;
        }
        assert_eq!(m.live_crews(), 5);
        let (m, c) = step(
            &m,
            &SupMsg::Spawn {
                spec: spec("f"),
                now_ms: 0,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::CapExceeded]);
        assert!(!m.children.contains_key("f"));
        // Spawn is idempotent: re-reading the registry adds nothing.
        let (m2, c) = step(
            &m,
            &SupMsg::Spawn {
                spec: spec("a"),
                now_ms: 9,
            },
        );
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
        assert_eq!(
            m.restarts.len(),
            1,
            "and it went through the intensity check"
        );
    }

    #[test]
    fn a_relaunch_that_failed_spends_the_budget_and_never_loops_free() {
        let (mut m, _) = step(&base(), &exit("a", 1_000));
        let mut esc = 0;
        for i in 0..6u64 {
            let key = format!("relaunch:a:{}", m.children["a"].generation);
            let (n, c) = step(
                &m,
                &SupMsg::CmdFailed {
                    key,
                    why: "x".into(),
                },
            );
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

    #[test]
    fn a_deaf_fact_ends_the_incarnation_once_however_often_it_is_read() {
        let d = |t| SupMsg::Deaf {
            child: "a".into(),
            key: "deaf:npub-a:4:no_answer".into(),
            now_ms: t,
        };
        let (m, c) = step(&base(), &d(1_000));
        assert_eq!(relaunches(&c), vec![("a".to_string(), 2)]);
        let (m, _) = step(
            &m,
            &SupMsg::Relaunched {
                child: "a".into(),
                generation: 2,
            },
        );
        let (m, c) = step(&m, &d(2_000));
        assert!(
            relaunches(&c).is_empty(),
            "the same fact is not a second death"
        );
        assert_eq!(m.children["a"].generation, 2);
        let fresh = SupMsg::Deaf {
            child: "a".into(),
            key: "deaf:npub-a:9:no_answer".into(),
            now_ms: 3_000,
        };
        let (_, c) = step(&m, &fresh);
        assert_eq!(
            relaunches(&c),
            vec![("a".to_string(), 3)],
            "a new fact for the new incarnation is"
        );
    }

    // ---- the pass child, wedge pages, dead stop (the locals cmd_loop_supervise kept)

    #[test]
    fn a_wedge_pages_once_until_work_moves() {
        let w = |p: &str| SupMsg::Wedged {
            pin: p.into(),
            line: "wedged".into(),
        };
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
        let s = SupMsg::Stalled {
            line: "dead stop".into(),
        };
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
        m = step(
            &m,
            &SupMsg::Spawn {
                spec: ChildSpec {
                    id: PASS.into(),
                    npub: None,
                    restart: Restart::Permanent,
                    kind: ChildKind::Crew,
                    role: None,
                },
                now_ms: 0,
            },
        )
        .0;
        m = step(
            &m,
            &SupMsg::PassExit {
                ok: false,
                now_ms: 1_000,
            },
        )
        .0;
        assert_eq!(m.restarts.len(), 1);
        m = step(
            &m,
            &SupMsg::PassExit {
                ok: true,
                now_ms: 2_000,
            },
        )
        .0;
        assert_eq!(m.children[PASS].abnormal, 0);
    }

    // ---- P8: replay; the view; serialization

    fn script() -> Vec<SupMsg> {
        vec![
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
            SupMsg::Control {
                op: SteerOp::Redirect,
                child: "a".into(),
                sup_epoch: 2,
                delta_sha: Some(D.into()),
                now_ms: 1_000,
            },
            SupMsg::SteerAppended { seq: 1, offset: 4 },
            exit("b", 2_000),
            SupMsg::Relaunched {
                child: "b".into(),
                generation: 2,
            },
            SupMsg::Progress {
                child: "b".into(),
                generation: 2,
                now_ms: 3_000,
            },
            SupMsg::Control {
                op: SteerOp::Cancel,
                child: "b".into(),
                sup_epoch: 1,
                delta_sha: None,
                now_ms: 3_500,
            },
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
            crew_view(&full)
                .iter()
                .map(|r| r.child.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"],
            "ordered by start order"
        );
    }

    fn lead_spec(id: &str) -> ChildSpec {
        let mut s = spec(id);
        s.kind = ChildKind::Lead;
        s
    }

    fn adopt(child: &str, epoch: u64, now_ms: u64) -> SupMsg {
        SupMsg::Adopt {
            child: child.into(),
            new_parent: "host-2".into(),
            sup_epoch: epoch,
            now_ms,
        }
    }

    /// A lead failure is one restart, not a free one, and the lead comes back at g+1.
    #[test]
    fn a_lead_failure_spends_one_restart_and_relaunches_at_the_next_generation() {
        let mut m = SupModel::default();
        for msg in [
            SupMsg::Hello {
                me: ME.into(),
                tier: Tier::Lead,
                policy: None,
            },
            SupMsg::Claim {
                holder: ME.into(),
                sup_epoch: 1,
            },
            SupMsg::Spawn {
                spec: lead_spec("plane"),
                now_ms: 0,
            },
        ] {
            m = step(&m, &msg).0;
        }
        let (m, c) = step(
            &m,
            &SupMsg::ChildEscalated {
                child: "plane".into(),
                reason: "intensity".into(),
                now_ms: 5_000,
                fact_key: None,
            },
        );
        assert_eq!(m.restarts, vec![5_000]);
        assert_eq!(relaunches(&c), vec![("plane".into(), 2)]);
        assert_eq!(m.children["plane"].generation, 2);
        assert_eq!(m.children["plane"].state, ChildState::Restarting);
        assert!(
            c.iter().all(|x| !matches!(x, SupCmd::Exit { .. })),
            "a lead is not a pane exit"
        );
    }

    /// Captain intensity is the same fence: the restart that would exceed MaxR escalates.
    #[test]
    fn a_captain_with_a_full_window_escalates_a_lead_instead_of_restarting_free() {
        let mut m = SupModel::default();
        for msg in [
            SupMsg::Hello {
                me: "captain".into(),
                tier: Tier::Captain,
                policy: None,
            },
            SupMsg::Claim {
                holder: "captain".into(),
                sup_epoch: 1,
            },
            SupMsg::Spawn {
                spec: lead_spec("plane"),
                now_ms: 0,
            },
            SupMsg::Spawn {
                spec: lead_spec("other"),
                now_ms: 0,
            },
        ] {
            m = step(&m, &msg).0;
        }
        assert_eq!(m.policy.max_r, CAPTAIN_MAX_R);
        // Two leads, so the third failure is still under the poison ceiling
        // and the captain window (2) is what stops it.
        for (child, t) in [("plane", 1_000u64), ("other", 2_000)] {
            let (n, c) = step(
                &m,
                &SupMsg::ChildEscalated {
                    child: child.into(),
                    reason: "down".into(),
                    now_ms: t,
                    fact_key: None,
                },
            );
            assert_eq!(
                relaunches(&c).len(),
                1,
                "inside the window the lead restarts"
            );
            let generation = n.children[child].generation;
            m = step(
                &n,
                &SupMsg::Relaunched {
                    child: child.into(),
                    generation,
                },
            )
            .0;
        }
        let (m, c) = step(
            &m,
            &SupMsg::ChildEscalated {
                child: "plane".into(),
                reason: "down".into(),
                now_ms: 90_000,
                fact_key: None,
            },
        );
        assert_eq!(escalations(&c), 1);
        assert!(
            relaunches(&c).is_empty(),
            "the over-budget restart is not made"
        );
        assert_eq!(m.status, Status::Failed);
        assert_eq!(m.restarts.len() as u32, CAPTAIN_MAX_R);
    }

    #[test]
    fn adopt_before_t_orphan_changes_nothing_and_after_it_the_old_epoch_is_stale() {
        let mut m = SupModel::default();
        let mut log = vec![
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
                spec: lead_spec("plane"),
                now_ms: 0,
            },
        ];
        for msg in &log {
            m = step(&m, msg).0;
        }
        assert_eq!(m.policy.t_orphan_ms, orphan_window_ms(DEFAULT_LEASE_MS));
        let t = m.policy.t_orphan_ms;
        let early = adopt("plane", 3, t - 1);
        let (m_early, c_early) = step(&m, &early);
        assert_eq!(m_early, m, "before T_orphan the model is untouched");
        assert!(c_early.is_empty());
        log.push(early);
        // A crew is not adoptable even after the window.
        let crew_try = adopt("nope", 3, t);
        let (m_crew, c_crew) = step(&m, &crew_try);
        assert_eq!(m_crew, m);
        assert!(c_crew.is_empty());
        let late = adopt("plane", 3, t);
        let (m, c) = step(&m, &late);
        assert!(c.is_empty(), "adoption is a model move, not a command");
        assert_eq!(m.accepted_epoch, 3);
        assert_eq!(m.leader, "host-2");
        assert_eq!(
            m.children["plane"].state,
            ChildState::Running,
            "crews were never stopped"
        );
        assert_eq!(m.children["plane"].generation, 1);
        log.push(late);
        let cancel = SupMsg::Control {
            op: SteerOp::Cancel,
            child: "plane".into(),
            sup_epoch: 2,
            delta_sha: None,
            now_ms: t + 1,
        };
        let (m, c) = step(&m, &cancel);
        assert_eq!(rejects(&c), vec![RejectCause::StaleEpoch]);
        assert_eq!(m.accepted_epoch, 3);
        log.push(cancel);
        assert_eq!(replay(&log).0, m, "the same log is the same model");
    }

    #[test]
    fn a_child_spec_without_kind_parses_as_crew() {
        let raw = r#"{"id":"a","npub":null,"restart":"transient"}"#;
        let s: ChildSpec = serde_json::from_str(raw).unwrap();
        assert_eq!(s.kind, ChildKind::Crew);
        let msg =
            r#"{"kind":"spawn","spec":{"id":"a","npub":null,"restart":"permanent"},"now_ms":0}"#;
        let parsed: SupMsg = serde_json::from_str(msg).unwrap();
        match parsed {
            SupMsg::Spawn { spec, .. } => assert_eq!(spec.kind, ChildKind::Crew),
            other => panic!("{other:?}"),
        }
        let policy = r#"{"tier":"lead","strategy":"one_for_one","max_r":3,"max_t_ms":1,"poison":3,"crew_cap":5,"ack_ms":1,"lease_ms":1}"#;
        let p: Policy = serde_json::from_str(policy).unwrap();
        assert_eq!(p.t_orphan_ms, default_t_orphan_ms());
    }

    #[test]
    fn a_lead_counts_toward_the_crew_cap_and_its_crews_are_not_in_this_model() {
        let mut m = base();
        m = step(
            &m,
            &SupMsg::Spawn {
                spec: lead_spec("plane"),
                now_ms: 0,
            },
        )
        .0;
        m = step(
            &m,
            &SupMsg::Spawn {
                spec: spec("c"),
                now_ms: 0,
            },
        )
        .0;
        m = step(
            &m,
            &SupMsg::Spawn {
                spec: spec("d"),
                now_ms: 0,
            },
        )
        .0;
        let (m, c) = step(
            &m,
            &SupMsg::Spawn {
                spec: spec("e"),
                now_ms: 0,
            },
        );
        assert_eq!(rejects(&c), vec![RejectCause::CapExceeded]);
        assert!(!m.children.contains_key("e"));
        assert_eq!(m.live_crews(), CREW_CAP);
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
        assert_eq!(
            k(&SupCmd::Relaunch {
                child: "a".into(),
                generation: 3,
                kind: ChildKind::Crew
            }),
            "relaunch:a:3"
        );
        assert_eq!(
            k(&SupCmd::AppendSteer {
                seq: 4,
                child: "a".into(),
                op: SteerOp::Cancel,
                generation: 1,
                sup_epoch: 2,
                urgent: true,
                delta_sha: None,
            }),
            "steer:4"
        );
        assert_eq!(
            k(&SupCmd::Assign {
                task: "t1".into(),
                child: "a".into(),
                generation: 1
            }),
            "assign:t1:a:1"
        );
    }

    fn bead(id: &str, path: &str, role: &str, proved: bool) -> ReadyBead {
        ReadyBead {
            id: id.into(),
            scope_paths: vec![path.into()],
            deps_proved: proved,
            role: role.into(),
        }
    }

    fn assigns(c: &[SupCmd]) -> Vec<(String, String)> {
        c.iter()
            .filter_map(|c| match c {
                SupCmd::Assign { task, child, .. } => Some((task.clone(), child.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn overlapping_beads_assign_the_first_and_a_disjoint_third() {
        // The overlap function the guard calls. Same path, conflict; different paths, not.
        assert!(toron_core::mailbox::reservations::overlaps(
            "src/a.rs", "src/a.rs"
        ));
        assert!(!toron_core::mailbox::reservations::overlaps(
            "src/a.rs", "src/b.rs"
        ));
        let msg = SupMsg::ReadyBeads {
            beads: vec![
                bead("t1", "src/a.rs", "a", true),
                bead("t2", "src/a.rs", "b", true),
                bead("t3", "src/b.rs", "b", true),
                bead("t4", "src/c.rs", "a", false),
            ],
            now_ms: 50,
        };
        let (m, c) = step(&base(), &msg);
        assert_eq!(
            assigns(&c),
            vec![("t1".into(), "a".into()), ("t3".into(), "b".into())]
        );
        assert!(!m.assignments.contains_key("t2"), "overlap stays pending");
        assert!(!m.assignments.contains_key("t4"), "deps not proved");
        let mut m2 = base();
        m2 = step(
            &m2,
            &SupMsg::Spawn {
                spec: lead_spec("plane"),
                now_ms: 0,
            },
        )
        .0;
        let (m2, c2) = step(
            &m2,
            &SupMsg::ReadyBeads {
                beads: vec![bead("lead-task", "src/z.rs", "plane", true)],
                now_ms: 1,
            },
        );
        assert!(assigns(&c2).is_empty(), "a lead is not a crew assignee");
        assert!(m2.assignments.is_empty());
    }

    #[test]
    fn ready_beads_failed_and_a_role_mismatch_assign_nothing() {
        let m = base();
        let (m2, c) = step(&m, &SupMsg::ReadyBeadsFailed { now_ms: 5 });
        assert_eq!(m2, m);
        assert!(c.is_empty());
        let (m3, c) = step(
            &m2,
            &SupMsg::ReadyBeads {
                beads: vec![bead("t9", "src/z.rs", "nope", true)],
                now_ms: 6,
            },
        );
        assert_eq!(m3, m2);
        assert!(assigns(&c).is_empty());
    }

    #[test]
    fn an_in_flight_assignment_consumes_a_crew_slot() {
        // Two live crews and cap 2: each takes one disjoint bead. A third
        // waits, because `a` is already busy and the book is full. Live
        // panes alone do not fill the book: a full spawn still receives work.
        let mut policy = Policy::for_tier(Tier::Lead);
        policy.crew_cap = 2;
        let mut m = SupModel::default();
        for msg in [
            SupMsg::Hello {
                me: ME.into(),
                tier: Tier::Lead,
                policy: Some(policy),
            },
            SupMsg::Claim {
                holder: ME.into(),
                sup_epoch: 1,
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
        let (m, c) = step(
            &m,
            &SupMsg::ReadyBeads {
                beads: vec![
                    bead("t1", "src/a.rs", "a", true),
                    bead("t3", "src/b.rs", "b", true),
                    bead("t5", "src/c.rs", "a", true),
                ],
                now_ms: 10,
            },
        );
        assert_eq!(
            assigns(&c),
            vec![("t1".into(), "a".into()), ("t3".into(), "b".into())]
        );
        assert!(!m.assignments.contains_key("t5"));
        let full = base();
        assert_eq!(full.live_crews(), 2);
        let (_, c) = step(
            &full,
            &SupMsg::ReadyBeads {
                beads: vec![bead("t1", "src/a.rs", "a", true)],
                now_ms: 1,
            },
        );
        assert_eq!(assigns(&c), vec![("t1".into(), "a".into())]);
    }

    #[test]
    fn ready_beads_replay_equals_the_model() {
        let log = vec![
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
            SupMsg::ReadyBeads {
                beads: vec![
                    bead("t1", "src/a.rs", "a", true),
                    bead("t2", "src/a.rs", "b", true),
                    bead("t3", "src/b.rs", "b", true),
                ],
                now_ms: 50,
            },
        ];
        let mut m = SupModel::default();
        for msg in &log {
            m = step(&m, msg).0;
        }
        assert_eq!(replay(&log).0, m);
        assert_eq!(m.assignments.len(), 2);
    }

    #[test]
    fn a_task_that_ends_abnormally_three_times_is_not_reassigned() {
        let mut wa = spec("a");
        wa.role = Some("worker".into());
        let mut wb = spec("b");
        wb.role = Some("worker".into());
        let mut log = vec![
            SupMsg::Hello {
                me: ME.into(),
                tier: Tier::Lead,
                policy: None,
            },
            SupMsg::Claim {
                holder: ME.into(),
                sup_epoch: 1,
            },
            SupMsg::Spawn {
                spec: wa,
                now_ms: 0,
            },
            SupMsg::Spawn {
                spec: wb,
                now_ms: 0,
            },
        ];
        let mut m = SupModel::default();
        for msg in &log {
            m = step(&m, msg).0;
        }
        for n in 1..=3u64 {
            let beads = SupMsg::ReadyBeads {
                beads: vec![bead("t1", "src/t.rs", "worker", true)],
                now_ms: n * 1_000,
            };
            let (next, c) = step(&m, &beads);
            assert_eq!(assigns(&c), vec![("t1".into(), "a".into())], "attempt {n}");
            log.push(beads);
            m = next;
            let ex = exit("a", n * 1_000 + 10);
            let (next, c) = step(&m, &ex);
            log.push(ex);
            m = next;
            if n < 3 {
                let generation = m.children["a"].generation;
                assert!(
                    relaunches(&c)
                        .iter()
                        .any(|(id, g)| id == "a" && *g == generation)
                );
                let rel = SupMsg::Relaunched {
                    child: "a".into(),
                    generation,
                };
                log.push(rel.clone());
                m = step(&m, &rel).0;
            } else {
                assert!(matches!(c.last(), Some(SupCmd::Park { child, .. }) if child == "a"));
                assert!(relaunches(&c).is_empty());
            }
        }
        assert_eq!(m.task_abnormal.get("t1").copied(), Some(3));
        assert!(m.parked_tasks.contains("t1"));
        assert_eq!(m.children["b"].state, ChildState::Running);
        let again = SupMsg::ReadyBeads {
            beads: vec![bead("t1", "src/t.rs", "worker", true)],
            now_ms: 9_000,
        };
        let before = m.clone();
        let (m2, c) = step(&m, &again);
        assert!(
            c.is_empty(),
            "the other worker does not inherit a poisoned task"
        );
        assert_eq!(m2, before);
        log.push(again);
        assert_eq!(replay(&log).0, m2);
    }

    fn proved_msg(id: &str, t: u64) -> SupMsg {
        SupMsg::Proved {
            child: id.into(),
            generation: 1,
            now_ms: t,
        }
    }

    /// P12. Join, integration, an empty conflict set, and released leases
    /// are one conjunction. A mutant that sets `goal_proved` from the join
    /// alone fails the assertion after the first `Proved`.
    #[test]
    fn p4b_close_goal_proved_needs_join_integration_clear_conflicts_and_released_leases() {
        let mut m = base();
        m.policy.join = Join::Quorum(1);
        let (m, c) = step(
            &m,
            &SupMsg::ReadyBeads {
                beads: vec![bead("t1", "src/a.rs", "a", true)],
                now_ms: 10,
            },
        );
        assert_eq!(assigns(&c), vec![("t1".into(), "a".into())]);
        let (m, c) = step(&m, &proved_msg("a", 20));
        assert!(
            c.iter()
                .any(|x| matches!(x, SupCmd::Exit { child, .. } if child == "b")),
            "quorum cancels the child that has not proved"
        );
        assert_eq!(m.children["b"].state, ChildState::Cancelled);
        assert!(
            m.assignments.is_empty(),
            "the proved child and the cancelled rest both release leases"
        );
        assert!(!m.goal_proved, "join alone is not the goal");
        let (m, _) = step(
            &m,
            &SupMsg::Integration {
                proved: true,
                now_ms: 30,
            },
        );
        assert!(m.goal_proved);
        let (m, _) = step(
            &m,
            &SupMsg::Conflict {
                key: "scope".into(),
                open: true,
                now_ms: 40,
            },
        );
        assert!(!m.goal_proved, "an open conflict blocks the goal");
        let (m, _) = step(
            &m,
            &SupMsg::Conflict {
                key: "scope".into(),
                open: false,
                now_ms: 50,
            },
        );
        assert!(m.goal_proved);

        let mut first = base();
        first.policy.join = Join::First(1);
        let (first, c) = step(&first, &proved_msg("a", 20));
        assert!(
            c.iter()
                .any(|x| matches!(x, SupCmd::Exit { child, .. } if child == "b"))
        );
        assert!(!first.goal_proved);

        let all = base();
        let (all, c) = step(&all, &proved_msg("a", 20));
        assert!(
            c.iter().all(|x| !matches!(x, SupCmd::Exit { .. })),
            "all does not cancel the child that has not proved"
        );
        let (all, _) = step(
            &all,
            &SupMsg::Integration {
                proved: true,
                now_ms: 30,
            },
        );
        assert!(!all.goal_proved, "all waits for every child");
        let (all, _) = step(&all, &proved_msg("b", 40));
        assert!(all.goal_proved);
    }

    #[test]
    fn p4b_close_usage_does_not_gate_and_wall_pauses_once() {
        let (m, c) = step(
            &base(),
            &SupMsg::Usage {
                child: "a".into(),
                input_tokens: 9,
                output_tokens: 8,
                usd_micros: u64::MAX,
                now_ms: 5,
            },
        );
        assert_eq!(m.status, Status::Running);
        assert!(c.is_empty());
        assert_eq!(m.usages["a"].usd_micros, u64::MAX);

        let mut m = base();
        m.policy.max_wall_ms = Some(100);
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: 1_000 });
        assert!(
            c.is_empty(),
            "the tick that starts the clock does not escalate"
        );
        assert_eq!(m.wall_origin_ms, 1_000);
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: 1_100 });
        assert_eq!(m.status, Status::Running);
        assert_eq!(
            c.iter()
                .filter(|x| matches!(x, SupCmd::Escalate { reason, .. } if reason == "budget"))
                .count(),
            1
        );
        assert!(m.children["a"].paused && m.children["b"].paused);
        let (m, c) = step(&m, &SupMsg::Tick { now_ms: 9_000 });
        assert!(
            c.iter().all(|x| !matches!(x, SupCmd::Escalate { .. })),
            "a second tick inside the same exhaustion does not page again"
        );
        assert!(m.budget_escalated);
    }

    #[test]
    fn p4b_close_halt_cancels_and_a_later_tick_does_not_relaunch() {
        let (m, c) = step(
            &base(),
            &SupMsg::Halt {
                sup_epoch: 2,
                now_ms: 50,
            },
        );
        assert_eq!(m.status, Status::Halted);
        assert!(!m.goal_proved);
        for id in ["a", "b"] {
            assert_eq!(m.children[id].state, ChildState::Cancelled);
            assert!(
                c.iter()
                    .any(|x| matches!(x, SupCmd::Exit { child, .. } if child == id))
            );
        }
        let (m2, c2) = step(&m, &SupMsg::Tick { now_ms: 9_000_000 });
        assert!(relaunches(&c2).is_empty());
        assert_eq!(m2.status, Status::Halted);
        let (again, c3) = step(
            &m,
            &SupMsg::Halt {
                sup_epoch: 2,
                now_ms: 60,
            },
        );
        assert!(c3.is_empty());
        assert_eq!(again, m);
        let (_, c4) = step(
            &m,
            &SupMsg::Halt {
                sup_epoch: 1,
                now_ms: 70,
            },
        );
        assert_eq!(rejects(&c4), vec![RejectCause::StaleEpoch]);
    }

    #[test]
    fn p4b_close_peer_grant_rejects_a_stale_epoch() {
        let grant = SupMsg::PeerGrant {
            a: "a".into(),
            b: "b".into(),
            verbs: "request,reply".into(),
            until_ms: 500,
            sup_epoch: 2,
            now_ms: 10,
        };
        let (m, c) = step(&base(), &grant);
        assert!(c.is_empty());
        assert_eq!(m.peers.len(), 1);
        let (m2, c2) = step(
            &m,
            &SupMsg::PeerGrant {
                a: "a".into(),
                b: "b".into(),
                verbs: "notify".into(),
                until_ms: 0,
                sup_epoch: 1,
                now_ms: 11,
            },
        );
        assert_eq!(rejects(&c2), vec![RejectCause::StaleEpoch]);
        assert_eq!(
            m2.peers.values().next().map(|p| p.verbs.as_str()),
            Some("request,reply")
        );
    }

    /// P14. Two accepted tasks with live leases never share a scope.
    /// Removing the overlap guard in `pick_assignee` assigns `t2` beside `t1`.
    #[test]
    fn p4b_close_two_accepted_tasks_never_overlap() {
        let (m, _) = step(
            &base(),
            &SupMsg::ReadyBeads {
                beads: vec![
                    bead("t1", "src/a.rs", "a", true),
                    bead("t2", "src/a.rs", "b", true),
                ],
                now_ms: 10,
            },
        );
        let rows: Vec<_> = m.assignments.values().collect();
        assert!(m.assignments.contains_key("t1"));
        assert!(!m.assignments.contains_key("t2"));
        for i in 0..rows.len() {
            for j in (i + 1)..rows.len() {
                assert!(
                    !scopes_overlap(&rows[i].scope_paths, &rows[j].scope_paths),
                    "{} overlaps {}",
                    rows[i].task,
                    rows[j].task
                );
            }
        }
    }

    #[test]
    fn p4b_close_bead_hold_argv_needs_a_bead() {
        assert!(bead_hold_argv("plain text", "budget").is_none());
        let v = bead_hold_argv("page bd-close1 then BD-SKIP", "also bd-close1").unwrap();
        assert_eq!(
            v,
            vec![
                "hold",
                "--kind",
                "captain",
                "--corr",
                "bd-close1",
                "bd-close1",
            ]
        );
    }
}

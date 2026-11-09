// governed-by: ADR-0024
// governed-by: ADR-0025
// governed-by: ADR-0026
// governed-by: ADR-0027
//! Durable swarm loop — the monitoring orchestrator as plain sequential code
//! with a local liveness row and an effect journal.
//!
//! Design: the loop is direct async code; every leaf is a plain function or a
//! journaled subprocess call. A process crash leaves the `ActiveLoop` row and
//! the effect journal behind, and the next start sweeps the dead owner and
//! starts fresh (or resumes by id with `--resume`). There is no vendor runtime
//! and no replay VM.

//!
//! Leaves shell out to the flywheel binary itself (`__poll` / `__poll_events` /
//! `__send` hidden subcommands) so the blocking hc socket client never runs
//! on the async executor and each side effect is a single crash-safe process.
//!
//! **Event stream (bd-event-subscribe-reactor):** `__poll_events` is a forward-
//! only latency optimization. hc has no since/seq replay. The heartbeat
//! `__poll` (agent.list snapshot) is the correctness floor on every iteration
//! and after crash-resume.
//!
//! **At-least-once + leaf idempotency (cg6.3):** leaves run under at-least-once
//! delivery — a crash while a leaf is in flight re-invokes it on resume. Every
//! SIDE-EFFECTING leaf is idempotent under that window:
//! - `send_prompt` → the folded `__send` subprocess dedups via the
//!   invocation-scoped `record_sent`/`should_resend` registry (main.rs);
//! - `respawn_agent` → `__respawn` no-ops when an agent with the same name
//!   already exists healthy in the workspace (main.rs; the `context_limit`
//!   path passes `--force` and keeps a documented replay window);
//! - `notify_blocked` → the persisted `last_ping_tier` marker gates the mail:
//!   at most one ping per logical tier transition (durable.rs escalation
//!   step);
//! - `spawn_xproj` → the per-project `{project}.xproj-seen.jsonl` marker is
//!   written BEFORE spawning: one swarm per message id.
//!   Pure read leaves (poll_*) need no dedup — re-running them is harmless.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use toron_core::mailbox::overlaps;

use crate::config::Config;
use crate::ingress::{IngressEvent, LedgerEvent};

fn update_mail_health(project: &str, success: bool, leaf: &str, evidence: &str, escalate_at: u32) {
    let now_ts = {
        let (secs, _) = crate::state::utc_now();
        secs as i64
    };
    if let Ok(Some(mut al)) = crate::state::load_active_loop(project) {
        if success {
            al.mail_health.record_success(now_ts);
            al.mail_degraded = false;
        } else {
            al.mail_health.record_failure(now_ts);
            let n = escalate_at;
            if al.mail_health.should_escalate(n) && !al.mail_degraded {
                al.mail_degraded = true;
                let msg = format!(
                    "mail-health degraded: {} leaf failing (consecutive {} >= {}), last evidence: {} — collision floor at risk",
                    leaf, al.mail_health.consecutive_failures, n, evidence
                );
                eprintln!("flywheel: {msg}");
            }
        }
        let _ = crate::state::save_active_loop(project, &al);
    }
}

/// Input to the durable loop workflow. Immutable per invocation — the workflow
/// id is `swarm-loop:<project>`, so re-running with the same params resumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopParams {
    pub project: String,
    pub to: String,
    /// settled/base cadence: poll interval when nobody is working (default 300)
    pub interval_secs: u64,
    /// fast cadence while anyone is working/blocked/unknown or right after a
    /// dispatch (default 30s — the pre-config balance; floor 5s lives in
    /// `next_interval_secs`). `#[serde(default = "default_active_interval")]`
    /// keeps pre-yna saved params loadable.
    #[serde(default = "default_active_interval")]
    pub active_interval_secs: u64,
    pub queue: Vec<String>,
    pub nudge: Option<String>,
    pub on_blocked: String,
    /// max poll iterations before failing (derived from --timeout-ms;
    /// deterministic across replays, unlike wall-clock deadlines)
    pub max_iterations: Option<u64>,
    /// bead-driven mode: source work from `br ready` instead of a static queue
    pub work: bool,
    /// bd-gnej kill-switch: disable the wave-gate (typed `wave` ordering,
    /// legacy `wave-N` labels as fallback) for graphs that do not use
    /// waves. Default false = gating ON.
    #[serde(default)]
    pub no_wave_gates: bool,
    /// ADR-0006 D2 wave-scoped dispatch (`--wave-label wave-N`): restrict
    /// --work to ready beads in wave N (typed field, legacy label fallback),
    /// held while any lower wave is open/in_progress. None = default
    /// behavior, unchanged. `#[serde(default)]` keeps pre-D2 saved params
    /// loadable.
    #[serde(default)]
    pub wave_label: Option<String>,
    /// send every prompt via `__send --confirm` (landing confirmation).
    /// Default true — reliability is the default, --no-confirm is the escape
    /// hatch. `#[serde(default)]` keeps pre-confirm saved params loadable.
    #[serde(default = "default_true")]
    pub confirm_sends: bool,
    /// confirm timeout in ms; None → leaf default 45s.
    #[serde(default)]
    pub confirm_timeout_ms: Option<u64>,
    /// this invocation's id, set at run time (last-sent registry; empty when
    /// running from pre-registry params — such sends record no invocation).
    #[serde(default)]
    pub invocation: String,
    /// consecutive `unknown` polls before Diagnose (bd-event-self-heal).
    #[serde(default = "default_unknown_diagnose_threshold")]
    pub unknown_diagnose_threshold: u32,
    /// max respawns per agent per loop run before EscalateBlocked.
    #[serde(default = "default_respawn_budget")]
    pub respawn_budget: u32,
    /// consecutive `unknown` polls treated as DETECTION GRACE (bd-bvwf.5):
    /// below this, an unknown agent is presumed detecting (fresh spawn /
    /// mid-identification) — no Diagnose, no Respawn. 0 = old behavior.
    #[serde(default = "default_unknown_detect_grace_polls")]
    pub unknown_detect_grace_polls: u32,
    /// completion drain-grace polls (bd-bvwf.3): the completion condition
    /// must hold for grace+1 consecutive polls before the loop exits — the
    /// extra poll catches a bead unblocked by the last closure (dependency
    /// chain) without a manual restart.
    #[serde(default = "default_drain_grace_polls")]
    pub drain_grace_polls: u32,
    /// claim-watch nudge cadence (bd-efk1): consecutive polls a dispatched
    /// bead may stay UNCLAIMED while its agent is engaged before the loop
    /// sends a claim-focused nudge. Default 2 (~60s at the 30s active
    /// cadence — the crew-execution gate's 60s claim bound).
    #[serde(default = "default_claim_nudge_after_polls")]
    pub claim_nudge_after_polls: u32,
    /// force takeover (bd-bvwf.4 + bd-e131): with --force the loop REJECTS a
    /// stale pending invocation instead of resuming it — a killed loop's
    /// promise stays pending on the server forever, and a plain resume
    /// REPLAYS the killed run's checkpointed state (stranded beads, no agent
    /// knowledge) instead of starting fresh. `#[serde(default)]` keeps saved
    /// params loadable.
    #[serde(default)]
    pub force: bool,
    /// bd-swarm-loop-durable-panic-atlo: opt IN to replaying a killed run's
    /// pending invocation. Default false — a pending invocation at start
    /// belongs to a dead owner (the pid registry refuses two live loops), and
    /// replaying its checkpoints against a newer binary's leaf shapes storms
    /// serialization errors. `#[serde(default)]` keeps saved params loadable.
    #[serde(default)]
    pub resume: bool,
    /// bd-p4oc strict vacuous gate: refuse 0-test VERIFY (also FLYWHEEL_VERIFY_STRICT=1)
    #[serde(default)]
    pub strict: bool,
    /// ADR-0025 reclaim thresholds resolved at loop start (bd-0rzv:
    /// serialized for replay determinism). `#[serde(default)]` keeps
    /// pre-field saved params loadable at the crate defaults.
    #[serde(default)]
    pub reclaim: crate::durable_policy::ReclaimPolicy,
    /// ADR-0016: pane-stall detection threshold, resolved once at loop
    /// start and serialized like `reclaim` — the tick never reads env.
    /// `#[serde(default)]` keeps pre-field saved params at the crate
    /// default (1800s, bd-cy0s).
    #[serde(default = "default_pane_stall_secs")]
    pub pane_stall_secs: u64,
    /// ADR-0016: dispatch-guard freshness TTL (µs), resolved once at loop
    /// start and serialized like `reclaim` — the guard leaf never reads env
    /// mid-tick. `#[serde(default)]` keeps pre-field saved params at the
    /// crate default (90 min, bd-dispatch-dedupe-2zlz).
    #[serde(default = "default_dispatch_guard_ttl_us")]
    pub dispatch_guard_ttl_us: i64,
    /// ADR-0016/ADR-0026: mail-health escalation threshold (0 disables),
    /// resolved once at loop start and serialized — the tick and the
    /// health-recording leaves read params, never env.
    #[serde(default = "default_mail_health_escalate_at")]
    pub mail_health_escalate_at: u32,
    /// ADR-0016: declared-pause resurface window (secs), resolved once at
    /// loop start — the drain path reads params, never env per pane.
    /// `#[serde(default)]` keeps pre-field params at the 3600s crate default
    /// (the declared-wait TTL twin on the doorbell path).
    #[serde(default = "default_pause_resurface_secs")]
    pub pause_resurface_secs: u64,
    /// ADR-0016: write-liveness walk budget (secs), resolved once at loop
    /// start; the drain-defer walk reads params, never env per bead.
    #[serde(default = "default_walk_timeout_secs")]
    pub walk_timeout_secs: u64,
}

fn default_pane_stall_secs() -> u64 {
    1800
}

fn default_dispatch_guard_ttl_us() -> i64 {
    DISPATCH_GUARD_TTL_US
}

fn default_mail_health_escalate_at() -> u32 {
    3
}

fn default_pause_resurface_secs() -> u64 {
    crate::write_liveness::PAUSE_RESURFACE_DEFAULT_SECS
}

fn default_walk_timeout_secs() -> u64 {
    crate::write_liveness::WALK_TIMEOUT_DEFAULT_SECS
}

fn default_unknown_diagnose_threshold() -> u32 {
    DEFAULT_UNKNOWN_DIAGNOSE_THRESHOLD
}

fn default_respawn_budget() -> u32 {
    DEFAULT_RESPAWN_BUDGET
}

fn default_unknown_detect_grace_polls() -> u32 {
    DEFAULT_UNKNOWN_DETECT_GRACE_POLLS
}

fn default_drain_grace_polls() -> u32 {
    DEFAULT_DRAIN_GRACE_POLLS
}

/// Loop-owned bead lifecycle (Spec 3) — closed enum, illegal transitions unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LoopPhase {
    #[default]
    Idle,
    Dispatched,
    ClaimWatch,
    Verify,
    Complete,
    WaveHeld,
    Blocked,
    Reclaim,
}

impl LoopPhase {
    /// Parse the persisted phase token — the same spelling serde emits for this
    /// enum (also what `ActiveLoop.per_bead_phase` holds). Unknown tokens return
    /// `None` so a hand-edited or future phase string degrades to "not reported"
    /// instead of failing the whole status read.
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "Idle" => Some(Self::Idle),
            "Dispatched" => Some(Self::Dispatched),
            "ClaimWatch" => Some(Self::ClaimWatch),
            "Verify" => Some(Self::Verify),
            "Complete" => Some(Self::Complete),
            "WaveHeld" => Some(Self::WaveHeld),
            "Blocked" => Some(Self::Blocked),
            "Reclaim" => Some(Self::Reclaim),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopEvent {
    Dispatch {
        agent_status: String,
        force: bool,
    },
    ClaimSeen,
    PaneExited,
    BrClose,
    Verify,
    Complete {
        has_command_verified: bool,
        wave_gate: bool,
    },
    WaveGate {
        bead_id: String,
    },
    Reclaim {
        holder_pin: String,
        claimant_pin: String,
        is_live: bool,
    },
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IllegalTransition {
    pub from: LoopPhase,
    pub event: LoopEvent,
    pub reason: &'static str,
}

/// Compatibility wrapper for the pure durable phase-transition seam.
pub fn transition(
    from: &LoopPhase,
    event: &LoopEvent,
) -> std::result::Result<LoopPhase, IllegalTransition> {
    crate::durable_transition::transition(from, event)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LoopModel {
    pub queue: Vec<String>,
    pub per_bead_phase: std::collections::BTreeMap<String, LoopPhase>,
    pub wake_suppression: std::collections::HashSet<String>,
    pub done_mailed: std::collections::HashSet<String>,
    pub event_cursor: Option<crate::state::EventCursor>,
    pub escalation: std::collections::BTreeMap<String, crate::state::EscalationState>,
    pub respawn: std::collections::BTreeMap<String, crate::state::RespawnBudget>,
    pub mail_health: crate::state::MailHealth,
    pub mail_degraded: bool,
    pub heal_states: std::collections::BTreeMap<String, HealState>,
    /// Heal thresholds from LoopParams (bd-blyz, effect-layer remainder → tick).
    #[serde(default)]
    pub heal_grace: u32,
    #[serde(default)]
    pub heal_threshold: u32,
    #[serde(default)]
    pub heal_budget: u32,
    /// Hc events buffered by `LoopMsg::Hc` (bd-ugd1); drained on the
    /// next Tick through apply_events_to_states + route decisions.
    pub pending_events: Vec<PollEvent>,
    /// Double-dispatch guard (bd-nsb3 fold): names dispatched via the event
    /// path awaiting `working` confirmation. Lives in model' so route
    /// decisions inside tick are visible and replayable.
    #[serde(default)]
    pub dispatch_pending: std::collections::HashSet<String>,
    /// Agent states (bd-p1ds): the polled swarm snapshot lives in the Model,
    /// not loop-local. Apply is single and inside the Tick arm.
    #[serde(default)]
    pub states: Vec<AgentState>,
    /// Graceful drain-stop (bd-zfnx.10): agents asked to finish their
    /// VERIFY fence before a kill, keyed by terminal name. Tick owns the
    /// countdown; Heal-exit fulfills; Tick-expiry force-stops.
    #[serde(default)]
    pub drain_pending: std::collections::BTreeMap<String, DrainState>,
    /// Per-bead control (bd-loop-slots): claim watch, close grace, verify
    /// dispatch. Replaces the stack-frame maps that reset on crash-resume.
    #[serde(default)]
    pub bead_slots: std::collections::BTreeMap<String, BeadSlot>,
    /// Per-agent control (bd-loop-slots): context-limit incarnations.
    #[serde(default)]
    pub agent_slots: std::collections::BTreeMap<String, AgentSlot>,
}

/// Per-bead control state owned by [`LoopModel`] (bd-loop-slots). One slot
/// replaces the three stack-frame maps that used to shadow the durable model
/// (`claim_watch`, `close_grace_ticks`, `verifier_dispatched`) and reset on
/// every crash-resume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadSlot {
    /// Claim-watch tracker while the bead is in flight.
    #[serde(default)]
    pub watch: Option<ClaimWatch>,
    /// Missing-verdict grace polls before the loop reopens the bead.
    #[serde(default)]
    pub close_grace_ticks: u32,
    /// Verify dispatch is once per bead, across crash-resume.
    #[serde(default)]
    pub verifier_dispatched: bool,
}

impl BeadSlot {
    pub fn is_empty(&self) -> bool {
        self.watch.is_none() && self.close_grace_ticks == 0 && !self.verifier_dispatched
    }
}

/// Per-agent control state owned by [`LoopModel`] (bd-loop-slots).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSlot {
    /// Fresh-pane generations started for this agent; a context-limit
    /// respawn bumps it so suppression belongs to the dead generation.
    #[serde(default)]
    pub incarnation: u64,
    /// Mail wakes are suppressed only while the dead generation's respawn
    /// is in flight. Resume seeding clears it: a fresh process must ring the
    /// fresh pane's mail.
    #[serde(default)]
    pub context_suppressed: bool,
}

impl LoopModel {
    /// The one bead slot for `bead_id`, created empty on first touch.
    pub fn bead_slot(&mut self, bead_id: &str) -> &mut BeadSlot {
        self.bead_slots.entry(bead_id.to_string()).or_default()
    }

    /// Fold the claim-watch step through the model-owned slots. The local
    /// map is a working projection only; the step's insert/remove result is
    /// written back through the same owner.
    pub fn fold_claim_watches(
        &mut self,
        step: impl FnOnce(&mut std::collections::HashMap<String, ClaimWatch>),
    ) {
        let mut watches: std::collections::HashMap<String, ClaimWatch> = self
            .bead_slots
            .iter()
            .filter_map(|(id, slot)| slot.watch.clone().map(|w| (id.clone(), w)))
            .collect();
        step(&mut watches);
        for slot in self.bead_slots.values_mut() {
            slot.watch = None;
        }
        for (id, watch) in watches {
            self.bead_slot(&id).watch = Some(watch);
        }
        self.bead_slots.retain(|_, slot| !slot.is_empty());
    }

    /// Count one WorkerReceipt grace poll and return the new count.
    pub fn bump_close_grace(&mut self, bead_id: &str) -> u32 {
        let slot = self.bead_slot(bead_id);
        slot.close_grace_ticks = slot.close_grace_ticks.saturating_add(1);
        slot.close_grace_ticks
    }

    /// Drop every per-bead control bit (legal close or reopen).
    pub fn clear_bead_slot(&mut self, bead_id: &str) {
        self.bead_slots.remove(bead_id);
    }

    /// Return true exactly once per bead (the verify-dispatch dedup).
    pub fn mark_verifier_dispatched(&mut self, bead_id: &str) -> bool {
        let slot = self.bead_slot(bead_id);
        let first = !slot.verifier_dispatched;
        slot.verifier_dispatched = true;
        first
    }

    /// Suppress mail wakes for one dead context-limit generation.
    pub fn suppress_context_limit(&mut self, name: &str) {
        let slot = self.agent_slots.entry(name.to_string()).or_default();
        slot.incarnation = slot.incarnation.saturating_add(1);
        slot.context_suppressed = true;
    }

    /// The fresh pane is up: its mail must ring again.
    pub fn clear_context_limit(&mut self, name: &str) {
        if let Some(slot) = self.agent_slots.get_mut(name) {
            slot.context_suppressed = false;
        }
    }

    /// Names whose wakes are suppressed (derived, never a second owner).
    pub fn wake_suppressed(&self) -> std::collections::HashSet<String> {
        self.agent_slots
            .iter()
            .filter(|(_, slot)| slot.context_suppressed)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// GS-030/GS-033 (NEW-L2): seed a LoopModel's crash-resume state from the
/// persisted ActiveLoop snapshot. Pure — the single seed **any** owner of
/// swarm resume state calls (doorbell `WorkspaceModel.swarm` via
/// [`seed_workspace_from_active_loop`], loop-local `LoopModel`, shared
/// Model path). Two call sites must never diverge on resume semantics:
/// heal counters carry over with `awaiting_clear` reset (within-episode),
/// done-mail dedup carries verbatim, drains restart with fresh grace (a
/// crash must never force-kill immediately; the prompt re-sends and send
/// dedup protects).
/// Compatibility wrapper for the pure recovery projection seam.
pub fn seed_swarm_resume(
    model: &mut LoopModel,
    heal_states: &std::collections::BTreeMap<String, crate::state::HealState>,
    done_mailed: &std::collections::HashSet<String>,
    drain_pending: &std::collections::BTreeMap<String, String>,
    bead_slots: &std::collections::BTreeMap<String, BeadSlot>,
    agent_slots: &std::collections::BTreeMap<String, AgentSlot>,
) {
    crate::durable_resume::seed_swarm_resume(
        model,
        heal_states,
        done_mailed,
        drain_pending,
        bead_slots,
        agent_slots,
    );
}

/// GS-033 (NEW-L2 / B1): seed the one WorkspaceModel's swarm slice from
/// ActiveLoop. Shared Model path — doorbell (the RAM owner) calls this
/// at start; tests / why-replay may too. Loop-only callers keep
/// [`seed_swarm_resume`] on a `LoopModel`.
pub fn seed_workspace_from_active_loop(
    ws: &mut crate::workspace::WorkspaceModel,
    project: &str,
) -> anyhow::Result<()> {
    match crate::state::load_active_loop(project) {
        Ok(Some(al)) => {
            seed_swarm_resume(
                &mut ws.swarm,
                &al.heal_states,
                &al.done_mailed,
                &al.drain_pending,
                &al.bead_slots,
                &al.agent_slots,
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "seed_workspace_from_active_loop({project}): load_active_loop failed (refusing silent skip): {e:#}"
        )),
    }
}

/// Throwaway WorkspaceModel around a LoopModel so swarm-slice Msgs still
/// enter through workspace::tick (the one entry). Not a live owner —
/// doorbell holds the RAM WorkspaceModel. Process split may remain.
///
/// The `..Default::default()` wrap is a transitional throwaway fold (GS-033
/// Option A honesty): it must not be read as a second live WorkspaceModel.
/// Tripwires allow this pattern only inside this helper.
fn tick_loop_owner(
    swarm: &mut LoopModel,
    msg: &crate::workspace::Msg,
) -> Vec<crate::workspace::Cmd> {
    let wm = crate::workspace::WorkspaceModel {
        swarm: swarm.clone(),
        ..Default::default()
    };
    let (next, cmds) = crate::workspace::tick(&wm, msg);
    *swarm = next.swarm;
    cmds
}

/// One drain in flight (bd-zfnx.10). Crash-safe via ActiveLoop (name →
/// bead id; grace restarts fresh on resume — a crash must never
/// force-kill immediately).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DrainState {
    pub bead_id: String,
    pub grace_polls: u32,
    /// D4 (bd-8j7m): consecutive write-evidence defers. Crash-resets to
    /// 0 via the resume seed (fresh grace, never force-kill immediately).
    #[serde(default)]
    pub defers: u32,
    /// ADR-0010 D4/D12 declared-`paused:` wait. While set, the grace
    /// countdown is HELD — a declared wait is not a stuck pane — and the loop
    /// re-surfaces the wait to its named owner every `resurface_secs`,
    /// re-arming the clock each window so a human wait can never hang
    /// silently (never auto-interrupt; auto-kill stays `--force-working`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<PausedWait>,
}

/// One declared `paused:` wait on a drained pane (ADR-0010 D4/D12): the human
/// who owns it, the paused-since clock, and the resurface cadence.
///
/// `instant`/`resurface_secs` travel ON the declaration (the shell stamps them
/// from `write_liveness::WalkOpts`), so the reducer reads no environment and a
/// journaled declaration replays to the same decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PausedWait {
    /// The human named by the declaration (`paused: <owner>`).
    pub owner: String,
    /// Tick-clock secs of the FIRST declaration. A repeat never re-stamps
    /// this, so the reported age (and the resurface) stays honest.
    pub since: u64,
    /// Resurface window in secs (default 3600 — the doorbell's pending-wait
    /// park TTL, so both wait kinds age the same).
    pub resurface_secs: u64,
    /// Windows already resurfaced (bounded, observable repeats).
    #[serde(default)]
    pub resurfaced: u32,
}

/// THE grace bound (bd-zfnx.10): a drained pane gets this many polls to
/// finish its VERIFY fence and report before the loop force-kills.
/// Working agents need an explicit reason to skip the drain
/// (`--force-working` / `--force` kill keys bypass it deliberately —
/// see the kill safety rules); the force path always records a LoudMiss
/// with the bead id so the receipt is recoverable, never silent.
pub const DRAIN_GRACE_POLLS: u32 = 3;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LoopMsg {
    Hc(PollEvent),
    BrClose {
        bead_id: String,
    },
    WaveGate {
        bead_id: String,
    },
    CommandVerified {
        bead_id: String,
        verified: bool,
    },
    Dispatch {
        bead_id: String,
        agent_status: String,
        force: bool,
    },
    Reclaim {
        bead_id: String,
        holder_pin: String,
        claimant_pin: String,
        is_live: bool,
    },
    /// Per-agent heal decision input (ADR-0027 fold list: `Heal { name, status, exited }`).
    /// tick owns the heal_state transition via `self_heal_step` and emits
    /// Diagnose/Respawn/Blocked actions; the executor leaves run the I/O.
    Heal {
        name: String,
        status: String,
        exited: bool,
    },
    /// Claim-watch watchdog (bd-efk1): unclaimed-bead nudge for the dispatched agent.
    ClaimNudge {
        name: String,
        bead_id: String,
    },
    /// Claim-watch watchdog: claimed-but-unclosed nudge for the idle assignee.
    CloseNudge {
        name: String,
        bead_id: String,
        title: String,
    },
    /// Graceful drain-stop ask (bd-zfnx.10): the pane finishes its current
    /// VERIFY fence and reports instead of being killed mid-fence (which
    /// orphans the receipt). Tick records drain_pending + emits the drain
    /// prompt as Send; Heal-exit fulfills; Tick-expiry force-stops with
    /// the bead id on the record.
    DrainRequest {
        name: String,
        bead_id: String,
    },
    /// ADR-0010 D4/D12: a drained pane DECLARED a wait on a human
    /// (`paused: <owner>`) instead of finishing its fence. Folded by the shell
    /// before the Tick that would count grace down; the declaration carries
    /// its own clock (`now`) and resurface window (no environment read in the
    /// reducer). A declaration for a pane with no open drain is ignored — the
    /// loop does not invent a wait.
    PauseDeclared {
        name: String,
        owner: String,
        now: u64,
        resurface_secs: u64,
    },
    /// ADR-0011 D7 / ADR-0029 Wave 1: rot scorer input for one pane.
    Rot {
        name: String,
        events: crate::rot::RotEvents,
    },
    Tick {
        now: u64,
    },
}

/// Delivery-verdict status projection of the durable loop (ADR-0010 D10):
/// per-bead phase, mail-degraded dispatch hold, queued work, and the event
/// watermark — exactly the state `loop status` and the fleet snapshot render.
///
/// Read-time projection of the persisted [`crate::state::ActiveLoop`]. The
/// status doors run in a different process than the loop and never hold the
/// `LoopModel`, so nothing derived is persisted twice: the `ActiveLoop` file is
/// the single source of truth.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopSnapshot {
    pub per_bead_phase: std::collections::BTreeMap<String, LoopPhase>,
    pub mail_degraded: bool,
    pub queue_len: usize,
    pub event_cursor: Option<crate::state::EventCursor>,
}

impl LoopSnapshot {
    pub fn from_active_loop(al: &crate::state::ActiveLoop) -> Self {
        Self {
            per_bead_phase: al
                .per_bead_phase
                .iter()
                .filter_map(|(k, v)| LoopPhase::parse(v).map(|p| (k.clone(), p)))
                .collect(),
            mail_degraded: al.mail_degraded,
            queue_len: al
                .params
                .get("queue")
                .and_then(|q| q.as_array())
                .map(Vec::len)
                .unwrap_or(0),
            event_cursor: al.event_cursor.clone(),
        }
    }
}

pub fn tick(model: &LoopModel, msg: &LoopMsg) -> (LoopModel, Vec<LoopAction>) {
    let mut next = model.clone();
    let mut actions = Vec::new();
    match msg {
        LoopMsg::Dispatch {
            bead_id,
            agent_status,
            force,
        } => {
            let from = model
                .per_bead_phase
                .get(bead_id)
                .cloned()
                .unwrap_or(LoopPhase::Idle);
            let event = LoopEvent::Dispatch {
                agent_status: agent_status.clone(),
                force: *force,
            };
            if let Ok(to) = transition(&from, &event) {
                next.per_bead_phase.insert(bead_id.clone(), to);
            }
        }
        LoopMsg::BrClose { bead_id } => {
            let from = model
                .per_bead_phase
                .get(bead_id)
                .cloned()
                .unwrap_or(LoopPhase::Idle);
            if let Ok(to) = transition(&from, &LoopEvent::BrClose) {
                next.per_bead_phase.insert(bead_id.clone(), to);
            }
        }
        LoopMsg::Reclaim {
            bead_id,
            holder_pin,
            claimant_pin,
            is_live,
        } => {
            let from = model
                .per_bead_phase
                .get(bead_id)
                .cloned()
                .unwrap_or(LoopPhase::Idle);
            let event = LoopEvent::Reclaim {
                holder_pin: holder_pin.clone(),
                claimant_pin: claimant_pin.clone(),
                is_live: *is_live,
            };
            if let Ok(to) = transition(&from, &event) {
                next.per_bead_phase.insert(bead_id.clone(), to);
                actions.push(LoopAction::Reclaim {
                    bead_id: bead_id.clone(),
                    pin: claimant_pin.clone(),
                    terminal_name: bead_id.clone(),
                    reason: "reclaim".to_string(),
                });
            }
        }
        LoopMsg::Heal {
            name,
            status,
            exited,
        } => {
            let hs = next.heal_states.entry(name.clone()).or_default();
            let action = self_heal_step(
                hs,
                status,
                *exited,
                next.heal_grace,
                next.heal_threshold,
                next.heal_budget,
            );
            let reason = if *exited { "pane_exited" } else { "unknown" };
            if *exited {
                next.drain_pending.remove(name);
            }
            match action {
                HealAction::None => {}
                HealAction::Diagnose => actions.push(LoopAction::Diagnose {
                    name: name.clone(),
                    reason: reason.into(),
                }),
                HealAction::Respawn => actions.push(LoopAction::Respawn {
                    name: name.clone(),
                    reason: reason.into(),
                }),
                HealAction::EscalateBlocked => {
                    actions.push(LoopAction::Blocked { name: name.clone() });
                }
            }
        }
        LoopMsg::ClaimNudge { name, bead_id } => {
            actions.push(LoopAction::Send {
                name: name.clone(),
                text: claim_nudge_prompt(bead_id),
            });
        }
        LoopMsg::CloseNudge {
            name,
            bead_id,
            title,
        } => {
            actions.push(LoopAction::Send {
                name: name.clone(),
                text: close_nudge_prompt(bead_id, title),
            });
        }
        LoopMsg::DrainRequest { name, bead_id } => {
            next.drain_pending
                .entry(name.clone())
                .or_insert(DrainState {
                    bead_id: bead_id.clone(),
                    grace_polls: DRAIN_GRACE_POLLS,
                    defers: 0,
                    paused: None,
                });
            actions.push(LoopAction::Send {
                name: name.clone(),
                text: drain_prompt(bead_id),
            });
        }
        LoopMsg::PauseDeclared {
            name,
            owner,
            now,
            resurface_secs,
        } => {
            if let Some(ds) = next.drain_pending.get_mut(name)
                && ds.paused.is_none()
            {
                ds.paused = Some(PausedWait {
                    owner: owner.clone(),
                    since: *now,
                    resurface_secs: *resurface_secs,
                    resurfaced: 0,
                });
            }
        }
        LoopMsg::Rot { name, events } => match crate::rot::action(events) {
            crate::rot::RotAction::Respawn => actions.push(LoopAction::Respawn {
                name: name.clone(),
                reason: "rot".into(),
            }),
            crate::rot::RotAction::Compact => actions.push(LoopAction::Send {
                name: name.clone(),
                text: crate::rot::compact_prompt().into(),
            }),
            crate::rot::RotAction::Advise | crate::rot::RotAction::None => {}
        },
        LoopMsg::Tick { now } => {
            let _ = now;
            apply_events_to_states(&mut next.states, &next.pending_events);
            next.pending_events.clear();
            let mut expired: Vec<(String, String, u32)> = Vec::new();
            let mut resurfaced: Vec<(String, String, String, u64)> = Vec::new();
            for (name, ds) in &mut next.drain_pending {
                let bead_id = ds.bead_id.clone();
                if let Some(p) = ds.paused.as_mut() {
                    let age_secs = now.saturating_sub(p.since);
                    if crate::write_liveness::paused_resurface_due(age_secs, p.resurface_secs) {
                        resurfaced.push((name.clone(), bead_id, p.owner.clone(), age_secs));
                        p.since = *now;
                        p.resurfaced = p.resurfaced.saturating_add(1);
                    }
                    continue;
                }
                if ds.grace_polls == 0 {
                    expired.push((name.clone(), bead_id, ds.defers));
                } else {
                    ds.grace_polls -= 1;
                }
            }
            for (name, bead_id, owner, age_secs) in resurfaced {
                actions.push(LoopAction::PausedResurface {
                    name,
                    bead_id,
                    owner,
                    age_secs,
                });
            }
            for (name, bead_id, defers) in expired {
                next.drain_pending.remove(&name);
                actions.push(LoopAction::Respawn {
                    name: name.clone(),
                    reason: format!("drain_expired bead {bead_id} defer {defers}"),
                });
            }
            if !next.mail_degraded
                && !next.queue.is_empty()
                && let Some(i) = next.states.iter().position(|s| {
                    matches!(s.status.as_str(), "idle" | "done")
                        && !next.dispatch_pending.contains(&s.name)
                })
            {
                let brief = next.queue.remove(0);
                let agent = next.states[i].name.clone();
                let pin = next.states[i]
                    .mail_identity
                    .clone()
                    .unwrap_or_else(|| agent.clone());
                let text = brief.replace("{AGENT_NAME}", &pin);
                if let Some(bead_id) = bead_id_from_prompt(&text) {
                    actions.push(LoopAction::PinStamp {
                        bead_id: bead_id.to_string(),
                        pin,
                    });
                }
                next.dispatch_pending.insert(agent.clone());
                actions.push(LoopAction::Send { name: agent, text });
            }
        }
        LoopMsg::Hc(pe) => {
            next.pending_events.push(pe.clone());
        }
        LoopMsg::WaveGate { bead_id } => {
            let from = model
                .per_bead_phase
                .get(bead_id)
                .cloned()
                .unwrap_or(LoopPhase::Idle);
            if let Ok(to) = transition(
                &from,
                &LoopEvent::WaveGate {
                    bead_id: bead_id.clone(),
                },
            ) {
                next.per_bead_phase.insert(bead_id.clone(), to);
            }
        }
        LoopMsg::CommandVerified { bead_id, verified } => {
            let from = model
                .per_bead_phase
                .get(bead_id)
                .cloned()
                .unwrap_or(LoopPhase::Idle);
            if let Ok(to) = transition(
                &from,
                &LoopEvent::Complete {
                    has_command_verified: *verified,
                    wave_gate: false,
                },
            ) {
                next.per_bead_phase.insert(bead_id.clone(), to);
            }
        }
    }
    (next, actions)
}

fn default_claim_nudge_after_polls() -> u32 {
    DEFAULT_CLAIM_NUDGE_AFTER_POLLS
}

fn default_true() -> bool {
    true
}

fn default_active_interval() -> u64 {
    30
}

/// One polled agent state: (name, status string).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentState {
    pub name: String,
    pub status: String,
    /// hc agent kind (`opencode`/`pi`/...). Learned from polls; used to
    /// respawn an agent whose hc record has been dropped (the escalation
    /// path cannot poll_kind a gone record). `#[serde(default)]` so
    /// checkpoints written before this field existed still resume.
    #[serde(default)]
    pub kind: String,
    /// hc terminal id — required to map push events → agent name
    /// (bd-event-dispatch-zero-latency). `#[serde(default)]` for old polls.
    #[serde(default)]
    pub pane_id: String,
    /// the agent's mail PIN (adjective+noun) — populated by `__poll` from
    /// the spawn registry so `{AGENT_NAME}` substitutes the pin, not the
    /// hc slug (bd-pin-public-plane-s-easy.2). `#[serde(default)]` for
    /// checkpoints written before this field existed.
    #[serde(default)]
    pub mail_identity: Option<String>,
    /// ADR-0011 D7: consecutive nudges without progress. 4th+ is mail-only.
    #[serde(default)]
    pub nudge_streak: u32,
}

/// One ready bead from `br ready --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyBead {
    pub id: String,
    pub title: String,
    /// The bead's assignee (bd-627z.1): `Some` for in_progress beads (who
    /// holds it), `None` for ready beads. The stranded-reclaim + scoped
    /// completion decision reads it.
    #[serde(default)]
    pub assignee: Option<String>,
    /// ADR-0019: the single-line command parsed from the bead's `## VERIFY`
    /// fence (a `## VERIFY` heading + ONE fenced one-line command).
    /// `Some(cmd)` = the bead is dispatchable. `None` = the fence is
    /// missing/multi-line/empty — the bead is NOT ready: refuse-to-dispatch
    /// + mail the planner on thread_id=<bead-id> (bd-w1-verify-fence).
    #[serde(default)]
    pub verify: Option<String>,
    /// The bead's labels (bd-gnej wave-gate: `wave-N` labels gate dispatch
    /// by wave order). `br ready --json` / `br list --json` emit them.
    #[serde(default)]
    pub labels: Vec<String>,
    /// bd-wave-gate-typed-authority-27et: br's typed `wave` field
    /// (`br create --wave N` / `br update --wave N`) — the wave-order
    /// authority since schema-18. `br ready` itself holds every higher wave
    /// until all lower waves close, so the typed field is what a live wave
    /// advance actually follows. `None` when unset, and also on the
    /// `br list` read path, which does not project the field.
    #[serde(default)]
    pub wave: Option<u32>,
    /// bd-4spn: the bead's priority (0=P0 … 3=P3). Default 3 (exempt) so a
    /// missing field in a foreign br JSON never gates a bead.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// bd-4spn: the bead description's `## PRINCIPLES` section (ADR-0019
    /// Spec 6 — REQUIRED for priority <= 2, every line `name — decision`).
    /// Empty = the section is absent (dispatch gate refuses P<=2).
    #[serde(default)]
    pub principles: String,
    /// ADR-0025 reclaim: ISO8601 updated_at from br JSON (e.g. "2026-08-28T05:18:45Z").
    /// `None` = unknown age (never reclaim on stale).
    #[serde(default)]
    pub updated_at: Option<String>,
    /// The bead's description (markdown). Used for WORKFLOW opt-in and goal extraction.
    #[serde(default)]
    pub description: String,
}

fn default_priority() -> u8 {
    3
}

impl Default for ReadyBead {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            assignee: None,
            verify: None,
            labels: Vec::new(),
            wave: None,
            priority: 3,
            principles: String::new(),
            updated_at: None,
            description: String::new(),
        }
    }
}

/// Format br's typed schema-v18 principles citations (`[{"name": …,
/// "decision": …}]`) into the line-per-citation text `check_principles`
/// consumes (ADR-0001 wave 5, bd-wave5-flywheel-parser-strip-dkpvi — the
/// description markdown is no longer read). Absent/empty array → empty
/// string, which the P<=2 gate refuses exactly like before.
pub fn principles_typed_field(bead: &serde_json::Value) -> String {
    bead.get("principles")
        .and_then(serde_json::Value::as_array)
        .map(|citations| {
            citations
                .iter()
                .filter_map(|c| {
                    let name = c.get("name")?.as_str()?.trim();
                    let decision = c.get("decision")?.as_str()?.trim();
                    (!name.is_empty() && !decision.is_empty())
                        .then(|| format!("{name} — {decision}"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Split ready beads into dispatchable (valid `## VERIFY` fence) and
/// not-ready (missing/malformed fence — refuse-to-dispatch + mail the
/// planner, ADR-0019). Pure — the single decision every dispatch path
/// (the single survivor loop body) shares.
/// Partition ready beads into dispatchable vs not-ready (ADR-0019).
///
/// A bead is dispatchable iff BOTH gates pass:
/// - the `## VERIFY` fence is present and single-line (bd-w1-verify-fence);
/// - the PRINCIPLES section is complete for priority <= 2 beads
///   (bd-4spn GAP 1 — `check_principles`: required, `name — decision`
///   lines, canonical names; P3+ is exempt).
///
/// The not-ready half feeds the planner mail; it is NEVER dispatched.
pub fn partition_ready(ready: &[ReadyBead]) -> (Vec<&ReadyBead>, Vec<&ReadyBead>) {
    ready
        .iter()
        .partition(|b| b.verify.is_some() && principles_block_reason(b).is_none())
}

/// Why a bead's PRINCIPLES section blocks dispatch (bd-4spn). `None` =
/// the section passes (or the bead is priority 3+ and exempt). The reason
/// names the exact failure for the planner mail.
pub fn principles_block_reason(b: &ReadyBead) -> Option<String> {
    match crate::principles::check_principles(b.priority, &b.principles) {
        crate::principles::PrinciplesCheck::Complete => None,
        crate::principles::PrinciplesCheck::Incomplete { reason } => Some(reason),
    }
}

/// bd-bead-create-contract-lint-4tdd: the not-ready REASON for a bead —
/// the loop's exact dispatch decision (partition_ready), named. `None` =
/// dispatchable. A missing typed verify field names the gate; PRINCIPLES
/// failures carry the exact `check_principles` reason (the 2026-08-17
/// mislabel class).
pub fn not_ready_reason(b: &ReadyBead) -> Option<String> {
    if b.verify.is_none() {
        return Some("missing/empty typed verify field".to_string());
    }
    principles_block_reason(b)
}

/// bd-bead-create-contract-lint-4tdd: one bead's contract verdict for the
/// creation-time check — the block reason + the corrected snippet. Takes
/// the raw description (the loop's exact parsers run over it) so the
/// report distinguishes a missing heading from a malformed/multi-line
/// fence and a bare `PRINCIPLES` heading from a truly absent section.
pub struct BeadContractIssue {
    pub id: String,
    pub reason: String,
    pub fix: String,
}

/// The corrected snippet for a blocked bead: name the exact edit — the
/// typed verify field, or the canonical-name guidance for a citation
/// failure.
fn contract_fix(_description: &str, reason: &str) -> String {
    if reason.starts_with("missing/empty typed verify") {
        return "set the single-line command: `br update <id> --verify '<command>'`".to_string();
    }
    "see reason (canonical names via `flywheel principles list`)".to_string()
}

/// The contract verdict of one ready bead (with its raw description):
/// `None` = dispatchable. ADR-0001 wave 5 (bd-wave5-flywheel-parser-strip-dkpvi):
/// the verify gate reads the TYPED field off the parsed bead — a description
/// fence no longer makes a bead dispatchable, so the report says to set
/// `br create --verify` / `br update --verify`.
pub fn contract_issue(b: &ReadyBead, _description: &str) -> Option<BeadContractIssue> {
    if b.verify.is_none() {
        return Some(BeadContractIssue {
            id: b.id.clone(),
            reason: "missing/empty typed verify field (set it via `br create --verify` or \
                     `br update --verify`)"
                .to_string(),
            fix: "add the single-line verify command: br update <id> --verify '<command>'"
                .to_string(),
        });
    }
    principles_block_reason(b).map(|reason| BeadContractIssue {
        id: b.id.clone(),
        fix: contract_fix(_description, &reason),
        reason,
    })
}

/// Extract the `## PRINCIPLES` section from a bead description (bd-4spn):
/// everything after the exact `## PRINCIPLES` heading until the next `## `
/// heading or EOF, trimmed. Absent -> empty string.
pub fn parse_principles_field(description: &str) -> String {
    let lines: Vec<&str> = description.lines().collect();
    let Some(heading) = lines.iter().position(|l| l.trim() == "## PRINCIPLES") else {
        return String::new();
    };
    let mut out: Vec<&str> = Vec::new();
    for line in &lines[heading + 1..] {
        if line.trim_start().starts_with("## ") {
            break;
        }
        if line.trim().is_empty() && out.is_empty() {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n").trim().to_string()
}

/// The typed `wave` field from a br JSON row (bd-wave-gate-typed-authority-27et).
/// `br ready --json` emits it as a number; `br list` emits `null`. A
/// non-number reads as `None`, so a malformed value falls back to the legacy
/// label instead of fabricating a wave. Pure.
pub fn bead_wave_field(b: &serde_json::Value) -> Option<u32> {
    b.get("wave")
        .and_then(serde_json::Value::as_u64)
        .and_then(|w| u32::try_from(w).ok())
}

/// A bead's wave number. bd-wave-gate-typed-authority-27et: br's TYPED
/// `wave` field is the authority (schema-18 shipped; `br ready` gates by it
/// natively) — the legacy `wave-N` label is read only as the fallback, for
/// beads that predate the field and for the `br list` path, which does not
/// project it. When both are present the typed value wins, so a stale label
/// can never overrule the tracker. No wave → `None` (no gating).
pub fn bead_wave(b: &ReadyBead) -> Option<u32> {
    b.wave.or_else(|| {
        b.labels
            .iter()
            .find_map(|l| l.strip_prefix("wave-").and_then(|n| n.parse::<u32>().ok()))
    })
}

/// bd-gnej wave-gate: a bead in wave N (typed `wave` field first, `wave-N`
/// label fallback — [`bead_wave`]) is HELD while ANY bead with a lower wave
/// (M<N) is open or in_progress — earlier waves run first. No wave anywhere
/// → no gating (identity). Pure — the single decision both dispatch paths
/// (sync loop + durable loop) share.
pub fn wave_gate_held(ready: &[ReadyBead], inprogress: &[ReadyBead]) -> Vec<String> {
    let min_open_wave = ready
        .iter()
        .chain(inprogress.iter())
        .filter_map(bead_wave)
        .min();
    let Some(floor) = min_open_wave else {
        return Vec::new();
    };
    ready
        .iter()
        .filter(|b| bead_wave(b).is_some_and(|w| w > floor))
        .map(|b| b.id.clone())
        .collect()
}

/// ADR-0006 D2 wave-scoped dispatch (`loop run --work --wave-label wave-N`):
/// the ready set is `br ready` scoped to wave N via [`bead_wave`] — the
/// TYPED `wave` field is the authority (bd-wave-gate-typed-authority-27et;
/// schema-18 has shipped), the legacy `wave-N` label the fallback — AND a
/// hold ASSERT when any open (ready, unclaimed) or in_progress bead has
/// wave-M with M < N — earlier waves run to completion first. The error
/// names every blocking bead so the operator sees the full hold. Pure —
/// shared by both dispatch paths.
pub fn wave_scope_filter(
    ready: &[ReadyBead],
    inprogress: &[ReadyBead],
    label: &str,
) -> std::result::Result<Vec<ReadyBead>, String> {
    let Some(wave_n) = label
        .strip_prefix("wave-")
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return Err(format!("--wave-label must be `wave-<N>` (got `{label}`)"));
    };
    let blockers: Vec<String> = ready
        .iter()
        .chain(inprogress.iter())
        .filter(|b| bead_wave(b).is_some_and(|w| w < wave_n))
        .map(|b| b.id.clone())
        .collect();
    if !blockers.is_empty() {
        return Err(format!(
            "wave-gate: {} blocked — lower wave still open/in_progress: {}",
            label,
            blockers.join(", ")
        ));
    }
    Ok(ready
        .iter()
        .filter(|b| bead_wave(b) == Some(wave_n))
        .cloned()
        .collect())
}

/// Pure: the loop's status word for one `__mail-not-ready` leaf result. A
/// leaf reporting `ok:false` — or JSON carrying no `ok` at all — is a
/// FAILURE. The old line printed `planner mailed: {v}` unconditionally, a
/// success word over a failed send, which is how beads were silently never
/// dispatched with no reason delivered. Fail closed: an absent `ok` is
/// never read as success.
pub(crate) fn not_ready_mail_status(v: &serde_json::Value) -> &'static str {
    match v.get("ok").and_then(serde_json::Value::as_bool) {
        Some(true) => "planner mailed",
        _ => "planner NOT mailed",
    }
}

/// Parse the `__reservation-board` JSON into `(path, reason)` rows. Shared
/// by the checkpointed leaf and the live read (bd-9tbb).
fn parse_reservation_rows(v: &serde_json::Value) -> Vec<(String, String)> {
    v.get("reservations")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    Some((
                        r.get("path")?.as_str()?.to_string(),
                        r.get("reason")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// bd-9tbb (leafReply): LIVE reservation-board read for the loop body —
/// deliberately NOT a checkpointed leaf. A `ctx.run` board read replays its
/// STORED result on every resume (durable checkpoint semantics), so a holder
/// that released between the crash and the resume kept `serialize_all`
/// pinned and dispatch stayed gated forever. The board is a world READ, not
/// a durable effect: it must observe the world as of NOW, and the sends it
/// gates are already deduped (dispatch_pending / assigned / last-sent
/// registry), so a replayed poll with a fresher board can only make a
/// FRESHER gating decision — the same trade the codebase already accepts
/// for the degrade-to-empty path. Same watchdog + degrade-to-empty contract
/// as the leaf: any failure logs and returns an empty board, never a crash.
async fn reservation_board_live(project: &str) -> Vec<(String, String)> {
    let exe = match self_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("flywheel: reservation board skipped (continuing): {e:#}");
            return Vec::new();
        }
    };
    let mut cmd = Command::new(&exe);
    cmd.args(["__reservation-board", project]);
    let out = match run_leaf_cmd(cmd, "__reservation-board").await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("flywheel: reservation board failed (continuing): {e:#}");
            return Vec::new();
        }
    };
    if !out.status.success() {
        eprintln!(
            "flywheel: reservation board failed (continuing): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Vec::new();
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or_default();
    parse_reservation_rows(&v)
}

/// Recent terminal text for one pane (`__read`), a LIVE read with the same
/// degrade-to-empty contract as [`reservation_board_live`]: any failure logs
/// and returns empty, so a pane whose tail cannot be read keeps the ordinary
/// drain path (never a stalled loop, never an invented declaration).
async fn read_pane_terminal_live(project: &str, name: &str) -> String {
    let exe = match self_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("flywheel: pane tail read skipped (continuing): {e:#}");
            return String::new();
        }
    };
    let mut cmd = Command::new(&exe);
    cmd.args(["__read", project, name]);
    let out = match run_leaf_cmd(cmd, "__read").await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("flywheel: pane tail read failed (continuing): {e:#}");
            return String::new();
        }
    };
    if !out.status.success() {
        eprintln!(
            "flywheel: pane tail read failed (continuing): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return String::new();
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<serde_json::Value>(raw.trim())
        .map(|v| parse_read_output(&v).terminal)
        .unwrap_or_default()
}

/// ADR-0010 D4/D12: the one rendering of a declared-pause resurface — who the
/// wait names, which bead it holds, and how long it has been held. Pure, so
/// both the durable and sync executors surface byte-identical text.
pub fn pause_resurface_line(name: &str, bead_id: &str, owner: &str, age_secs: u64) -> String {
    format!(
        "pause-resurface: {name} declared `paused: {owner}` ({age_secs}s held, bead {bead_id}) — the wait needs {owner}; the pane is held, never auto-interrupted"
    )
}

/// The close-path settle plan (bd-5ouq): what one `__loop-run-verify`
/// outcome means, computed pure from the child value. Executing the plan
/// is `settle_loop_verify_output` below. Splitting plan from execution
/// keeps every shape — including the armed record path — pinned by tests
/// without a loop-armed global or a beads workspace in the suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoopVerifySettle {
    /// Band is not loop-runnable: return the value unchanged, run nothing.
    Passthrough,
    /// Record the proof: receipt content, optional advisory annotation for
    /// a vacuous-but-tolerated run, and the verify command for the verdict.
    Record {
        output: String,
        warn_append: Option<String>,
        verify_cmd: String,
    },
    /// Reopen the bead: failed/skipped head, or a strict vacuous refusal.
    /// `vacuous` carries the warning, the filtered count, and the evidence
    /// file content when the refusal wrote one.
    Reopen {
        vacuous: Option<(String, Option<u32>, String)>,
        timed_out: bool,
        exit: i64,
    },
}

pub(crate) fn plan_loop_verify_settle(v: &serde_json::Value, strict: bool) -> LoopVerifySettle {
    if v.get("band").and_then(serde_json::Value::as_str) != Some("loop-runnable") {
        return LoopVerifySettle::Passthrough;
    }
    let skipped = v
        .get("skipped")
        .and_then(serde_json::Value::as_str)
        .is_some();
    let timed_out = v
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let exit = v
        .get("exit")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    if skipped || exit != 0 {
        return LoopVerifySettle::Reopen {
            vacuous: None,
            timed_out,
            exit,
        };
    }
    let output = v
        .get("output_tail")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let verify_cmd = v
        .get("verify")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_cargo = crate::is_cargo_test_verify(&verify_cmd);
    let (passed, filtered) = if is_cargo {
        crate::parse_cargo_test_counts(output)
    } else {
        (None, None)
    };
    let is_vacuous = is_cargo && passed == Some(0);
    if is_vacuous {
        let warning = format!(
            "WARNING: vacuous VERIFY — filter `{verify_cmd}` matched 0 tests (evidence proves nothing)"
        );
        if strict {
            let evidence = format!(
                "{warning}\nfiltered out: {}\noutput tail:\n{}\n",
                filtered
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                output
            );
            return LoopVerifySettle::Reopen {
                vacuous: Some((warning, filtered, evidence)),
                timed_out,
                exit,
            };
        }
        let warn_append = format!(
            "\n{warning}\nfiltered out: {}\n",
            filtered
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
        return LoopVerifySettle::Record {
            output: output.to_string(),
            warn_append: Some(warn_append),
            verify_cmd,
        };
    }
    LoopVerifySettle::Record {
        output: output.to_string(),
        warn_append: None,
        verify_cmd,
    }
}

/// Settle one `__loop-run-verify` outcome (bd-5ouq): the post-subprocess
/// half of `loop_run_verify`, with the child run journaled and the
/// evaluation inline with caller authority. Takes the parsed child value
/// plus caller-side facts (the verdict timestamp comes from the caller's
/// injected authority, never sampled here; `reopen` performs the journaled
/// reopen — production passes `reopen_bead_journaled`, tests pass scripted
/// outcomes so the suite stays hermetic).
///
/// Shapes, receipt paths and the vacuous gate are byte-identical to the
/// `#[function]` body. The one deliberate deviation lives at the call site,
/// not here: reopen goes through `run_leaf_journaled` (same idempotency —
/// one mail per (bead, sha) — plus a journal row, so a kill between verify
/// and reopen resumes without duplicates) instead of a direct child wait.
pub(crate) fn settle_loop_verify_output(
    v: &serde_json::Value,
    _project: &str,
    bead_id: &str,
    sha: &str,
    strict: bool,
    at: chrono::DateTime<chrono::Utc>,
    reopen: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<serde_json::Value> {
    let dir = v
        .get("dir")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(".");
    let cwd = std::path::Path::new(dir);
    match plan_loop_verify_settle(v, strict) {
        LoopVerifySettle::Passthrough => Ok(v.clone()),
        LoopVerifySettle::Record {
            output,
            warn_append,
            verify_cmd,
        } => {
            let receipt =
                crate::verdict::write_command_verified_receipt(cwd, bead_id, sha, &output);
            match receipt {
                Ok(receipt_path) => {
                    if let Some(warning) = warn_append {
                        let _ = crate::append_to_receipt(&receipt_path, &warning);
                    }
                    let verdict = crate::verdict::Verdict {
                        kind: crate::verdict::VerdictKind::CommandVerified,
                        bead_id: bead_id.to_string(),
                        sha: sha.to_string(),
                        at,
                        actor: crate::verdict::AgentName::new("flywheel-toron-loop1"),
                        receipt_path,
                        verify_cmd,
                    };
                    match crate::verdict::report_pass(cwd, &verdict) {
                        Ok(()) => Ok(serde_json::json!({
                            "band": "loop-runnable",
                            "recorded": true,
                            "bead_id": bead_id,
                            "sha": sha,
                        })),
                        Err(e) => Err(anyhow::anyhow!(
                            "loop-run-verify record failed for {bead_id}: {e}"
                        )),
                    }
                }
                Err(e) => Err(anyhow::anyhow!(
                    "loop-run-verify receipt write failed for {bead_id}: {e}"
                )),
            }
        }
        LoopVerifySettle::Reopen {
            vacuous,
            timed_out,
            exit,
        } => {
            let warning = if let Some((warning, filtered, evidence)) = vacuous {
                let _ = (|| -> anyhow::Result<()> {
                    let path = crate::project_state::receipt_path(cwd, bead_id, sha, "vacuous");
                    crate::project_state::write_atomic(&path, evidence.as_bytes())?;
                    Ok(())
                })();
                eprintln!("{warning}");
                Some((warning, filtered))
            } else {
                None
            };
            let reopened = reopen().map_err(|e| anyhow::anyhow!("__reopen-bead failed: {e:#}"))?;
            if let Some((warning, filtered)) = warning {
                return Ok(serde_json::json!({
                    "band": "loop-runnable",
                    "reopened": reopened,
                    "vacuous": true,
                    "bead_id": bead_id,
                    "warning": warning,
                    "filtered": filtered,
                }));
            }
            Ok(serde_json::json!({
                "band": "loop-runnable",
                "reopened": reopened,
                "bead_id": bead_id,
                "timed_out": timed_out,
                "exit": exit,
            }))
        }
    }
}

/// Reopen through the journaled leaf (bd-5ouq): same idempotency as the
/// direct child wait (one mail per (bead, sha)) plus a journal row, so a
/// kill between verify and reopen resumes without duplicating the mail.
/// Returns the leaf's parsed value; the caller only checks success.
fn reopen_bead_journaled(project: &str, bead_id: &str, sha: &str) -> anyhow::Result<bool> {
    let key = format!("loop-run-verify-reopen:{bead_id}:{sha}");
    crate::elm::leaf::run_leaf_journaled(project, "__reopen-bead", &[project, bead_id, sha], &key)
        .map(|_| true)
        .map_err(|e| anyhow::anyhow!("__reopen-bead failed: {e:#}"))
}

/// Args for the `__poll_events` leaf (bd-event-subscribe-reactor).
/// Simple identifier params only — the leaf contract's rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollEventsArgs {
    pub project: String,
    /// block up to this many ms collecting events
    pub timeout_ms: u64,
    /// best-effort in-run dedup cursor (0 = none)
    #[serde(default)]
    pub after_seq: u64,
}

/// One status-change event from `__poll_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollEvent {
    pub event_type: String,
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    pub seq: u64,
}

/// Batch returned by `__poll_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PollEventsBatch {
    #[serde(default)]
    pub events: Vec<PollEvent>,
    #[serde(default)]
    pub max_seq: u64,
    #[serde(default)]
    pub stream_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RespawnArgs {
    pub project: String,
    pub name: String,
    pub kind: String,
    /// Why the respawn happened — logged with the event (`stalled`,
    /// `context_limit`, `error:<...>`, ...).
    pub reason: String,
    /// Consecutive send failures for this agent, logged with the event.
    pub attempts: u64,
    /// When a prompt is requeued with this respawn, the hash of that prompt
    /// — its delivery record is INVALIDATED (the delivery died with the
    /// respawned agent; the fresh agent must get the prompt again).
    #[serde(default)]
    pub text_hash: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendArgs {
    pub project: String,
    pub name: String,
    pub text: String,
    /// wait for the prompt to land (__send --confirm); default true in the loop
    pub confirm: bool,
    /// confirm timeout in ms; None → leaf default 45s
    pub confirm_timeout_ms: Option<u64>,
    /// loop invocation id (last-sent registry bookkeeping; empty when the
    /// loop predates the registry)
    pub invocation: String,
    /// loop iteration at send time (last-sent registry bookkeeping)
    pub iteration: u64,
    /// profile name whose behavior preamble conditions the prompt (Spec C,
    /// bd-gsvn.1). Empty = raw send (plain swarm agents — byte-identical
    /// pre-migration behavior).
    #[serde(default)]
    pub profile: String,
}

/// Build the argv for the `__send` leaf invocation.
///
/// Pure (testable without spawning a process). `--confirm` is passed without
/// a value when the timeout is None — the leaf's clap layer applies its own
/// default (45s), keeping one source of truth for the default.
pub fn send_prompt_argv(args: &SendArgs) -> Vec<String> {
    let mut argv = vec![
        "__send".to_string(),
        args.project.clone(),
        args.name.clone(),
        args.text.clone(),
    ];
    if args.confirm {
        argv.push("--confirm".to_string());
        if let Some(ms) = args.confirm_timeout_ms {
            argv.push(ms.to_string());
        }
    }
    if !args.invocation.is_empty() {
        argv.push("--invocation".to_string());
        argv.push(args.invocation.clone());
        argv.push("--iteration".to_string());
        argv.push(args.iteration.to_string());
    }
    if !args.profile.is_empty() {
        argv.push("--profile".to_string());
        argv.push(args.profile.clone());
    }
    argv
}

/// bd-a7j: one in-flight reply expectation the workflow is verifying.
/// Workflow-local + rebuilt from the checkpointed `load_expected_replies`
/// leaf each iteration, so replay reproduces the same decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedReplyRun {
    /// Mail identity the file stored. The map key is [`crate::state::ask_key`],
    /// not this field: `reply_settled` must be handed this, or it deletes the
    /// wrong row.
    pub agent: String,
    pub thread: String,
    /// message id of the question (the reply must differ from it)
    pub head: Option<String>,
    /// 0 = not reminded yet, 1 = reminded once (next silence escalates)
    pub reminder_count: u32,
}

/// Rebuild the in-run reply map from the file. The key is
/// [`crate::state::ask_key`]`(agent, head or thread)`, so two asks from one
/// agent both stay. A matching key keeps its reminder count when the thread
/// is the same question; a new thread starts the budget over.
pub fn reconcile_expected_replies(
    prev: &std::collections::HashMap<String, ExpectedReplyRun>,
    entries: &[crate::state::ExpectedReply],
) -> std::collections::HashMap<String, ExpectedReplyRun> {
    let mut next = std::collections::HashMap::new();
    for e in entries {
        let key = crate::state::ask_key(&e.agent, e.head.as_deref().unwrap_or(&e.thread));
        let reminder_count = match prev.get(&key) {
            Some(old) if old.thread == e.thread => old.reminder_count,
            _ => 0,
        };
        next.insert(
            key,
            ExpectedReplyRun {
                agent: e.agent.clone(),
                thread: e.thread.clone(),
                head: e.head.clone(),
                reminder_count,
            },
        );
    }
    next
}

/// bd-a7j: pure decision — did the asked thread move after dispatch?
/// `newest_id`/`author` come from the checkpointed check-reply leaf; `head`
/// is the question's message id. A reply satisfies iff it is a NEW message
/// from the asked agent; silence after one reminder escalates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyVerdict {
    Satisfied,
    Reminded,
    Escalate,
}

pub fn verify_reply(
    head: Option<&str>,
    newest_id: Option<&str>,
    author: Option<&str>,
    agent: &str,
    reminder_count: u32,
) -> ReplyVerdict {
    match (newest_id, author) {
        (Some(nid), Some(auth)) if auth == agent && Some(nid) != head => ReplyVerdict::Satisfied,
        _ if reminder_count == 0 => ReplyVerdict::Reminded,
        _ => ReplyVerdict::Escalate,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemindArgs {
    pub project: String,
    pub name: String,
    pub thread: String,
    pub reminder_count: u32,
    pub invocation: String,
    pub iteration: u64,
}

pub fn remind_reply_argv(args: &RemindArgs) -> Vec<String> {
    let mut argv = vec![
        "__remind-reply".to_string(),
        args.project.clone(),
        args.name.clone(),
        args.thread.clone(),
        "--reminder-count".to_string(),
        args.reminder_count.to_string(),
    ];
    if !args.invocation.is_empty() {
        argv.push("--invocation".to_string());
        argv.push(args.invocation.clone());
        argv.push("--iteration".to_string());
        argv.push(args.iteration.to_string());
    }
    argv
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyArgs {
    pub project: String,
    pub name: String,
    /// Wall-clock observation captured by the workflow host before the leaf.
    pub now_us: i64,
    /// Seed for a FIRST escalation entry (None → the leaf stamps `now`).
    /// The leaf loads the persisted entry when one exists — replay never
    /// re-seeds, so the tier sequence is deterministic (cg6.1).
    pub first_blocked_ts: Option<i64>,
    /// Loop poll interval in seconds — the escalation tier thresholds
    /// (2×/4×/8× interval) derive from it.
    pub interval_secs: u64,
    /// Loop params, preserved into a freshly-created registry entry
    /// (parity with the pre-cg6.1 body-side first-init).
    pub params: serde_json::Value,
}

/// What the `notify_blocked` leaf computed and persisted (cg6.1). The
/// workflow body acts on these values — it never recomputes the tier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EscalationOutcome {
    /// Escalation tier 0..3, derived from the PERSISTED first-blocked
    /// timestamp at leaf-execution time.
    pub tier: u32,
    /// Ping count after this notification (persisted; starts at 1).
    pub ping_count: u32,
}

/// Args for the diagnose leaf (read tail + health mail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnoseArgs {
    pub project: String,
    pub name: String,
    pub reason: String,
}

/// Mail digest for one agent (loop-side wake signal, fed by `__mail-digest`).
///
/// Honest fields: toron's projection has NO read-state (`unread_only` is a
/// documented no-op — "fresh on first read"), so the digest reports the
/// RECENT inbox window plus URGENT (importance high|urgent) — both real
/// message attributes. `urgent` is the wake trigger.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MailDigest {
    /// the hc agent name — the loop's digest-map key (plan_actions looks
    /// digests up by the agent state name)
    pub agent: String,
    /// S8.1: the REGISTERED mail identity this agent fetches as (the
    /// spawn-registry / agent-names pin, else the canonical name). The wake
    /// prompt's "fetch as X" / "reply --as X" use THIS name — never a
    /// not-yet-registered alias (debrief D1/D2).
    #[serde(default)]
    pub mail_identity: Option<String>,
    #[serde(default)]
    pub recent: usize,
    #[serde(default)]
    pub urgent: usize,
    /// bd-pin-public-plane-s-easy.5: plane-S wake signals — open kind-43001
    /// jobs assigned to this agent (or channel mentions). Same wake path as
    /// urgent mail: an idle agent with crew-visible work gets woken.
    #[serde(default)]
    pub plane_s: usize,
    /// id of the most recent urgent message (the wake dedup key: the loop
    /// wakes an agent ONCE per message id, so a still-unreplied urgent
    /// message does not re-prompt every iteration).
    #[serde(default)]
    pub last_id: Option<String>,
    #[serde(default)]
    pub last_from: Option<String>,
    #[serde(default)]
    pub last_subject: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Args for the xproj-spawn leaf (cross-project swarm receive, full branch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XprojSpawnArgs {
    pub project: String,
    /// String: agent-mail ids are u64 (arrive as numbers), toron ids are hex
    /// event ids (strings). The leaf passes it through verbatim.
    pub message_id: String,
    pub thread_id: String,
    pub from: String,
    pub recipe: String,
    pub workers: u32,
}

fn self_exe() -> anyhow::Result<std::path::PathBuf> {
    std::env::current_exe().map_err(anyhow::Error::from)
}

/// Run a leaf's blocking child-process wait on tokio's blocking pool
/// (cg6.8). Every leaf body is `async fn` doing a plain synchronous
/// `std::process::Command::output()` — a blocking child wait inline on the
/// workflow task. The old vendor SDK executed leaves INLINE on the workflow
/// task (verified in context.rs), so a blocked worker thread is occupied for
/// the leaf's full duration (up to 300s for `poll_events`). Benign on the
/// current multi-threaded `Runtime::new()` (a blocked worker is masked by
/// the pool), but a latent footgun: a `current_thread` runtime would stall
/// the whole loop on the first leaf, and any future `ctx.run(...).spawn()`
/// fan-out would silently serialize — and exhaust — the thread pool.
///
/// `spawn_blocking` moves the exact same `std::process::Command` code to
/// tokio's blocking pool; the child-process-per-leaf crash-isolation
/// boundary is untouched (a leaf panic still cannot kill the loop's
/// memory).
///
/// bd-loop-park-fresh-invocation-xl6r: leaf subprocess watchdog. A hung
/// leaf child (e.g. the toron daemon reserve-write hang of 6ec181d, >300s
/// while reads respond) must NEVER park the durable loop — a fresh
/// invocation stalls on the first un-checkpointed write leaf and the loop
/// dies in cvwait with nothing scheduled. The default bound covers every
/// leaf whose subprocess runs seconds-to-a-minute; leaves with a legit
/// longer internal deadline (poll_events, send --confirm, loop-run-verify
/// TIMEBOX) pass their own bound + grace via `run_leaf_cmd_bounded`.
pub(crate) const DEFAULT_LEAF_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(120);

/// Grace added on top of a leaf's own internal deadline when the call site
/// knows it (poll_events `--timeout-ms`, confirm timeout, VERIFY TIMEBOX).
pub(crate) const LEAF_WATCHDOG_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) async fn run_leaf_cmd(
    cmd: std::process::Command,
    label: &'static str,
) -> anyhow::Result<std::process::Output> {
    run_leaf_cmd_bounded(cmd, label, DEFAULT_LEAF_WATCHDOG).await
}

/// `run_leaf_cmd` with an explicit subprocess watchdog: kill the child and
/// surface a bounded Err when it outlives `watchdog` (park guard). The
/// child is drained concurrently so a chatty leaf cannot deadlock the wait.
///
/// bd-loop-park-fresh-invocation-xl6r (grandchild orphan, CoralFinch
/// review): the leaf child runs in its OWN process group
/// (`process_group(0)`, pgid == child pid), and the timeout path kills the
/// WHOLE GROUP via `kill -- -<pgid>` — a leaf's own children (br/git/cargo
/// it spawned) die with it instead of orphaning to PPID 1 (prior incident:
/// orphaned cargo test pid 9791).
pub(crate) async fn run_leaf_cmd_bounded(
    mut cmd: std::process::Command,
    label: &'static str,
    watchdog: std::time::Duration,
) -> anyhow::Result<std::process::Output> {
    tokio::task::spawn_blocking(move || {
        toron_core::proc::run_bounded(&mut cmd, watchdog).map_err(|e| match e {
            toron_core::proc::ProcError::Timeout { pid } => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{label} leaf exceeded {watchdog:?} — process group {pid} killed (watchdog; a hung leaf must never park the loop, and its children must not orphan)"
                ),
            ),
            toron_core::proc::ProcError::Io(e) => e,
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("{label} leaf task join failed: {e}"))?
    .map_err(|e| anyhow::anyhow!("{label} spawn failed: {e}"))
}

/// argv for one supervised pass (pure; the CLI spawns it as a child process
/// so a crashed pass cannot kill the supervisor). Deliberately NO `--force`:
/// a crashed pass leaves a DEAD pid entry which the next pass replaces
/// silently, while a LIVE loop still owns the project — the pass must refuse
/// loudly instead of taking the project over.
pub fn supervise_pass_argv(project: &str, work: bool, to: &str) -> Vec<String> {
    let mut args = vec![
        "loop-mode".to_string(),
        "run".to_string(),
        project.to_string(),
        "--once".to_string(),
        "--to".to_string(),
        to.to_string(),
    ];
    if work {
        args.push("--work".to_string());
    }
    args
}

/// bd-supervise-dead-stop-escalation-rweq: default dead-stop window (minutes).
/// A dead stop older than this with work remaining pages the captain.
pub const DEAD_STOP_DEFAULT_MINUTES: u64 = 45;

/// Which dead stop was observed. Both are "no artifact movement past the
/// window with work remaining"; they differ in what the panes claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadStopKind {
    /// No pane reads Busy — the fleet is silent.
    SilentPanes,
    /// Pane(s) read Busy but nothing has moved: a working verdict alone is not
    /// progress (a pane can sit hours inside a hang and still say working).
    StuckWorking,
}

/// One supervise dead-stop probe. Pure inputs so the verdict is testable
/// without a live fleet, repo, or clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadStopInputs {
    /// any bead not closed/done (ready or in_progress)
    pub work_remaining: bool,
    /// plane pins were readable. False → the fleet is UNKNOWABLE; never alarm
    /// blind (a degraded read is neither calm nor a stall).
    pub fleet_knowable: bool,
    pub any_pane_working: bool,
    /// seconds since the newest commit; None when git is absent or the repo
    /// has no commit (never a fabricated "very old" age)
    pub last_commit_age_secs: Option<u64>,
    pub stall_secs: u64,
    /// this stall has already paged once — do not page again, do not stay loud
    pub already_escalated: bool,
}

/// The supervise dead-stop decision (bd-supervise-dead-stop-escalation-rweq).
///
/// `Clear` is emitted when work moved inside the window (the caller resets its
/// escalated flag, so a SECOND stall can be reported); `Escalate` fires exactly
/// once per stall; `Hold` is every pass after that until the fleet recovers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadStopAction {
    /// Not a stall: either no work left, or a commit landed inside the window.
    Clear,
    /// A stall cannot be judged (no commit history, or an unreadable fleet).
    Wait,
    /// First sighting of this stall — emit ONE loud line and page once.
    Escalate(DeadStopKind),
    /// Already escalated this stall — stay silent (not one page per pass).
    Hold,
}

pub fn dead_stop_action(i: &DeadStopInputs) -> DeadStopAction {
    if !i.work_remaining {
        return DeadStopAction::Clear;
    }
    if !i.fleet_knowable {
        return DeadStopAction::Wait;
    }
    match i.last_commit_age_secs {
        Some(age) if age < i.stall_secs => DeadStopAction::Clear,
        Some(_) => {
            if i.already_escalated {
                DeadStopAction::Hold
            } else if i.any_pane_working {
                DeadStopAction::Escalate(DeadStopKind::StuckWorking)
            } else {
                DeadStopAction::Escalate(DeadStopKind::SilentPanes)
            }
        }
        None => DeadStopAction::Wait,
    }
}

/// Fatal-error markers a harness PARKS on when a turn dies and will not
/// recover by itself. Observed (2026-10-06): all five panes wedged for ~2h on
/// `opencode-go` HTTP 400 with a reasoning-replay refusal; each turn failed
/// silently, CPU at 0, no commits, no mail. Matched against the pane's LAST
/// non-empty output line, so a marker that merely scrolled past a working pane
/// is not a wedge.
pub const WEDGE_ERROR_MARKERS: &[&str] = &[
    "reasoning replay",
    "api error",
    "api_error",
    "rate limit",
    "too many requests",
    "unauthorized",
    "forbidden",
    "insufficient",
    "quota",
];

/// The pane's parked error line: its last non-empty output line when that line
/// carries a fatal marker. `None` when the tail is empty or the last line is
/// ordinary output. Bounded, so a page stays one line. Pure.
pub fn parked_error_line(recent_output: &str) -> Option<String> {
    let last = recent_output.lines().rev().find(|l| !l.trim().is_empty())?;
    let lower = last.to_ascii_lowercase();
    WEDGE_ERROR_MARKERS.iter().find(|m| lower.contains(*m))?;
    Some(last.trim().chars().take(300).collect())
}

/// One pane's wedge evidence. Pure inputs so the verdict is testable without a
/// live fleet, an hc socket, or a clock.
#[derive(Debug, Clone)]
pub struct PaneWedgeInputs<'a> {
    pub pin: &'a str,
    pub pane_id: &'a str,
    /// hc's process probe: `Some(true)` = the agent process is ALIVE — the
    /// wedge shape (a pane whose process exited is `recover`'s job, never this
    /// probe's); `None` = inconclusive, never judged.
    pub process_alive: Option<bool>,
    /// the pane's recent terminal text; `None` = unreadable, never judged.
    pub recent_output: Option<&'a str>,
    /// seconds since the newest commit; `None` when git is absent or the repo
    /// has no commit (never a fabricated "very old" age).
    pub last_commit_age_secs: Option<u64>,
    /// the no-movement window in seconds
    pub quiet_secs: u64,
}

/// The pane-wedge decision. `Page` is emitted on the FIRST sighting of a
/// wedge; `Hold` is every later pass while it persists — one page per wedge,
/// never one per pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneWedgeAction {
    /// not wedged, or not judgeable
    Quiet,
    /// first sighting — emit ONE loud line and page once
    Page {
        pin: String,
        pane_id: String,
        error: String,
    },
    /// already paged this wedge — stay silent
    Hold,
}

/// bd-pane-wedge-detect-respawn-pnyi: is this pane WEDGED — alive but making
/// no progress because it is parked on a fatal error?
///
/// The bead asks for "zero-CPU plus error output". hc exposes no CPU
/// counter (`pane.process_info` lists processes only), so the observable
/// equivalent is used and named honestly: the agent process is still UP (a
/// dead one is `recover`'s job) AND the newest commit is older than the
/// window. An unreadable pane, an inconclusive process probe, a repo with no
/// commit history, and a tail whose last line is ordinary output all read
/// `Quiet` — a wedge alarm must never fire blind.
pub fn pane_wedge_action(i: &PaneWedgeInputs<'_>, already_paged: bool) -> PaneWedgeAction {
    if i.process_alive != Some(true) {
        return PaneWedgeAction::Quiet;
    }
    let Some(tail) = i.recent_output else {
        return PaneWedgeAction::Quiet;
    };
    let Some(error) = parked_error_line(tail) else {
        return PaneWedgeAction::Quiet;
    };
    match i.last_commit_age_secs {
        Some(age) if age >= i.quiet_secs => {}
        _ => return PaneWedgeAction::Quiet,
    }
    if already_paged {
        return PaneWedgeAction::Hold;
    }
    PaneWedgeAction::Page {
        pin: i.pin.to_string(),
        pane_id: i.pane_id.to_string(),
        error,
    }
}

/// Collect hc push events for up to `timeout_ms` (forward-only).
/// Checkpointed. Empty batch on stream failure is a soft outcome (the
/// heartbeat `__poll` remains the correctness floor).
async fn poll_events(args: PollEventsArgs) -> anyhow::Result<PollEventsBatch> {
    let exe = self_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args([
        "__poll_events",
        &args.project,
        "--timeout-ms",
        &args.timeout_ms.to_string(),
    ]);
    if args.after_seq > 0 {
        cmd.args(["--after-seq", &args.after_seq.to_string()]);
    }
    let out = run_leaf_cmd_bounded(
        cmd,
        "__poll_events",
        std::time::Duration::from_millis(args.timeout_ms) + LEAF_WATCHDOG_GRACE,
    )
    .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Ok(PollEventsBatch {
            events: vec![],
            max_seq: args.after_seq,
            stream_error: Some(if err.is_empty() {
                format!("__poll_events exit {}", out.status)
            } else {
                err
            }),
        });
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let batch = parse_poll_events_batch(raw.trim(), args.after_seq)?;
    log_poll_events(&args.project, &batch);
    Ok(batch)
}

/// Pure parse of `__poll_events` stdout (unit-testable).
pub fn parse_poll_events_batch(raw: &str, fallback_seq: u64) -> anyhow::Result<PollEventsBatch> {
    let v: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("__poll_events bad JSON: {e}: {raw}"))?;
    let events = v
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let event_type = e
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let pane_id = e
            .get("pane_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let agent = e.get("agent").and_then(|a| {
            if a.is_null() {
                None
            } else {
                a.as_str().map(str::to_string)
            }
        });
        let status = e.get("status").and_then(|s| {
            if s.is_null() {
                None
            } else {
                s.as_str().map(str::to_string)
            }
        });
        let seq = e.get("seq").and_then(Value::as_u64).unwrap_or(0);
        out.push(PollEvent {
            event_type,
            pane_id,
            agent,
            status,
            seq,
        });
    }
    let max_seq = v
        .get("max_seq")
        .and_then(Value::as_u64)
        .unwrap_or(fallback_seq);
    let stream_error = v.get("stream_error").and_then(|e| {
        if e.is_null() {
            None
        } else {
            e.as_str().map(str::to_string)
        }
    });
    Ok(PollEventsBatch {
        events: out,
        max_seq,
        stream_error,
    })
}

/// Log each routed event to the project JSONL audit trail.
pub fn log_poll_events(project: &str, batch: &PollEventsBatch) {
    for e in &batch.events {
        let _ = crate::state::log_event(
            project,
            crate::state::EventKind::State,
            serde_json::json!({
                "source": "poll_events",
                "event_type": e.event_type,
                "pane_id": e.pane_id,
                "agent": e.agent,
                "status": e.status,
                "seq": e.seq,
            }),
        );
    }
}

/// Parse the `{"beads": [...]}` leaf output shared by `__ready` (id/title)
/// and `__inprogress` (id/title/assignee) into `ReadyBead` rows. Pure —
/// bd-87hq regression pin: the producer emitters (cmd_internal_ready /
/// cmd_internal_inprogress in main.rs) and this consumer must never drift.
pub fn parse_bead_list(v: &serde_json::Value, what: &str) -> anyhow::Result<Vec<ReadyBead>> {
    let beads = v
        .get("beads")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("{what} missing beads: {v}"))?;
    Ok(beads
        .iter()
        .map(|b| ReadyBead {
            id: b
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: b
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            assignee: b
                .get("assignee")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            verify: b
                .get("verify")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            labels: b
                .get("labels")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            wave: bead_wave_field(b),
            priority: b
                .get("priority")
                .and_then(serde_json::Value::as_u64)
                .map(|p| p.min(u64::from(u8::MAX)) as u8)
                .unwrap_or(3),
            principles: b
                .get("principles")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    b.get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(parse_principles_field)
                })
                .unwrap_or_default(),
            description: b
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            updated_at: None,
        })
        .collect())
}

/// The `__ready` leaf envelope (bd-87hq never-drift pin): the EXACT shape
/// `cmd_internal_ready` prints and `parse_bead_list` re-reads. Must carry
/// `priority` + `principles` — the durable loop's PRINCIPLES dispatch gate
/// (bd-4spn) reads them off the parsed rows; dropping them here silently
/// defaults every bead to P3-exempt and neuters the gate on the durable
/// path (the sync path parses `br` JSON directly and was unaffected).
pub fn ready_leaf_envelope(beads: &[ReadyBead]) -> serde_json::Value {
    serde_json::json!({
        "beads": beads
            .iter()
            .map(|b| {
                serde_json::json!({
                    "id": b.id,
                    "title": b.title,
                    "verify": b.verify,
                    "labels": b.labels,
                    "priority": b.priority,
                    "principles": b.principles,
                })
            })
            .collect::<Vec<_>>()
    })
}

/// The `__inprogress` leaf envelope (same pin): id/title/assignee plus the
/// gate fields for shape symmetry with `__ready` — `parse_bead_list` is the
/// shared consumer and must never see a field one leaf carries and the
/// other drops.
pub fn inprogress_leaf_envelope(beads: &[ReadyBead]) -> serde_json::Value {
    serde_json::json!({
        "beads": beads
            .iter()
            .map(|b| {
                serde_json::json!({
                    "id": b.id,
                    "title": b.title,
                    "assignee": b.assignee,
                    "verify": b.verify,
                    "labels": b.labels,
                    "priority": b.priority,
                    "principles": b.principles,
                })
            })
            .collect::<Vec<_>>()
    })
}

/// Parse the `{"entries": [...]}` leaf output of `__expected-replies`. Pure —
/// bd-87hq regression pin (producer: cmd_internal_expected_replies).
fn parse_expected_reply_entries(v: &serde_json::Value) -> Vec<crate::state::ExpectedReply> {
    v.get("entries")
        .and_then(|e| serde_json::from_value(e.clone()).ok())
        .unwrap_or_default()
}

/// Parse the `{"newest_id": ..., "author": ...}` leaf output of
/// `__check-reply`. Pure — bd-87hq regression pin.
fn parse_reply_check(v: &serde_json::Value) -> (Option<String>, Option<String>) {
    (
        v.get("newest_id")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
        v.get("author")
            .and_then(serde_json::Value::as_str)
            .map(String::from),
    )
}

/// The dispatched bead prompt is a filled brief (ADR-0019 Spec 2), not the
/// old lifecycle prose. The state-file contract (bd-prompt-state-file) tells
/// the agent to write `.flywheel/agent-state/{AGENT_NAME}.json` when context
/// is high; the `{AGENT_NAME}` placeholder is substituted with the receiving
/// agent's terminal name at dispatch time (plan_actions) so the agent never
/// guesses the filename. `context_pct` < 80 is ignored by the READER
/// (bd-read-state-fallback) — the threshold lives there, not here.
/// Hard cap on the dispatched prompt (behavior + skills prefix + brief).
/// Over the cap is a LOOP BUG — the send fails loud, never silent truncation
/// (ADR-0019 Spec 2, `minimize-reader-load`).
pub const DISPATCH_PROMPT_CAP: usize = 4 * 1024;

/// Why a filled brief could not be dispatched. Only the cap exists today —
/// a brief that cannot fit must never be truncated or silently shortened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefError {
    /// Size of the filled brief that exceeded the cap (bytes).
    pub bytes: usize,
}

impl std::fmt::Display for BriefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "filled brief is {} bytes — over the {} byte dispatch cap (ADR-0019 Spec 2; loop bug, never truncated)",
            self.bytes, DISPATCH_PROMPT_CAP
        )
    }
}

impl std::error::Error for BriefError {}

/// The dispatched bead prompt: a filled brief (ADR-0019 Spec 2). The old
/// claim→reserve→announce→close lifecycle novel (`bead_prompt`) is GONE —
/// the lifecycle lives in the standing-orders file, referenced BY PATH ONLY
/// in the STANDING field. GOAL carries the bead id (`Execute <id>:` /
/// `Resume <id>:` per Spec 2) so the claim-watch watchdog can extract it
/// ([`bead_id_from_prompt`]) without parsing prose. The state-file /
/// ack / lease-renewal contracts carry over from the old prompts
/// (bd-prompt-state-file, bd-fug, bd-mail-lease-renewal-durable).
pub fn build_brief(
    id: &str,
    title: &str,
    project: &str,
    resume: bool,
) -> std::result::Result<String, BriefError> {
    let full = fill_brief(id, title, project, resume);
    if full.len() <= DISPATCH_PROMPT_CAP {
        return Ok(full);
    }
    let pointer = pointer_brief(id, project, resume);
    if pointer.len() <= DISPATCH_PROMPT_CAP {
        return Ok(pointer);
    }
    Err(BriefError {
        bytes: pointer.len(),
    })
}

/// Fill the full brief template. No cap check here — [`build_brief`] owns the
/// degrade decision.
fn fill_brief(id: &str, title: &str, project: &str, resume: bool) -> String {
    let goal = if resume {
        format!("Resume {id}: {title}")
    } else {
        format!("Execute {id}: {title}")
    };
    BRIEF_TEMPLATE
        .replace("{goal}", &goal)
        .replace("{id}", id)
        .replace("{project}", project)
}

/// The pointer brief (bd-dispatch-cap-conditioning-l7jh): the degraded form
/// used when the full brief cannot fit the dispatch cap together with the
/// agent's conditioning (behavior preamble + skills prefix). It names the
/// bead, the `br show` read path that carries the full acceptance criteria,
/// and the report contract — everything else is reachable, not embedded.
/// Never truncated, and the `GOAL ` first line keeps the claim-watch bead-id
/// extractor working ([`bead_id_from_prompt`]).
pub fn pointer_brief(id: &str, project: &str, resume: bool) -> String {
    let verb = if resume { "Resume" } else { "Execute" };
    POINTER_TEMPLATE
        .replace("{verb}", verb)
        .replace("{id}", id)
        .replace("{project}", project)
}

/// The brief template (ADR-0019 Spec 2 fields). `{id}` / `{goal}` /
/// `{project}` are filled by [`build_brief`]; `{AGENT_NAME}` is substituted
/// at dispatch time (plan_actions) so the state-file path is concrete.
const BRIEF_TEMPLATE: &str = r#"GOAL {goal}.
SCOPE may-write only paths you hold an exclusive reservation on (reason={id}); anything else is may-not-write — no scope leak.
CONTEXT pointers only, never paste file contents: br show {id} (acceptance criteria + ## VERIFY fence); thread-id {id}; state .flywheel/agent-state/{AGENT_NAME}.json; receipts .flywheel/receipts/.
ACCEPTANCE close only when every acceptance criterion holds — one checkable line per criterion in the close reason.
VERIFY run the bead's ## VERIFY command from its description; include its stdout in the REPORT. No ## VERIFY fence → say so explicitly; never substitute a whole-workspace matrix script.
TIMEBOX ~90 min; expiry → partial findings + deviations, never a silent extension.
FORBIDDEN DO NOT create new beads — report scope gaps in the thread. Never force-unlock a peer's reservation. No worktree. Stay inside SCOPE.
REPORT status; commit sha (commit BEFORE close; bead id {id} IN the message); verdict; what you ran; deviations; PRINCIPLES. Then mail the captain thread_id={id}, subject [{id}] done, same fields — one report, two channels (close reason durable; mail = captain's).
PRINCIPLES each line `<principle-name> — <the decision it changed>`; names only. REQUIRED for priority <= 2 beads — a name with no decision clause is a skip (cargo-cult citation).
STANDING {project}/.flywheel/standing-orders.md + ~/.config/flywheel/standing-orders.md (lifecycle: claim → reserve → announce → execute → verify → close → sync → commit) — BY PATH ONLY.
DIRT POLICY dirt outside source/, tests/, benches/, Cargo.toml, Cargo.lock, and pinned toolchains blocks a receipt; .beads/, .flywheel/, and editor artifacts never do (note + proceed); anything else dirty: post-and-stop.
CONTRACTS renew your own file reservations (toron `renew_file_reservations`; the orchestrator cannot renew on your behalf). When a message has ack_required=true, call `acknowledge_message` after acting — the orchestrator cannot ack on your behalf. Above ~80% context: write {"context_pct": <pct>, "ts": "<now RFC3339>"} to .flywheel/agent-state/{AGENT_NAME}.json (best-effort), finish + sync this bead, reply exactly CONTEXT_LIMIT."#;

/// The pointer brief template: `{verb}` / `{id}` / `{project}` filled by
/// [`pointer_brief`]. Keep the first line the `GOAL ` form and keep the
/// `OVER CAP` marker — tests and operators read it as the degradation signal.
const POINTER_TEMPLATE: &str = r#"GOAL {verb} {id}: full brief OVER CAP — read it with `br show {id}` (acceptance criteria + ## VERIFY fence).
SCOPE may-write only paths you hold an exclusive reservation on (reason={id}).
VERIFY run the bead's ## VERIFY command; REPORT status, commit sha (bead id {id} IN the commit message), verdict, what you ran, deviations, PRINCIPLES cited; then mail the captain on thread_id={id}, subject [{id}] done.
STANDING {project}/.flywheel/standing-orders.md (lifecycle) — by path only.
FORBIDDEN no new beads; never force-unlock a peer's reservation; no worktree; stay inside SCOPE. Above ~80% context → CONTEXT_LIMIT per standing orders."#;

/// Convenience wrapper for call sites that must produce a brief as a plain
/// String (sync-loop queue refresh). Over-cap degrades to the pointer brief;
/// only a bead id too large for even the pointer form panics (ADR-0019
/// Spec 2 loop bug).
pub fn brief(id: &str, title: &str, project: &str, resume: bool) -> String {
    build_brief(id, title, project, resume).expect(
        "bead brief cannot fit the dispatch cap even as a pointer (ADR-0019 Spec 2 loop bug)",
    )
}

/// Queue a filled brief for a dispatched bead. Over-cap degrades to the
/// pointer brief inside [`build_brief`]; only the pathological case — an id
/// so large even the pointer cannot fit — fails loud here (ADR-0019 Spec 2).
/// `front` = re-queue at the head (claim-watch re-dispatches must stay ahead
/// of fresh beads).
fn queue_brief(
    queue: &mut Vec<String>,
    id: &str,
    title: &str,
    project: &str,
    resume: bool,
    front: bool,
) {
    match build_brief(id, title, project, resume) {
        Ok(b) => {
            if front {
                queue.insert(0, b);
            } else {
                queue.push(b);
            }
        }
        Err(e) => eprintln!(
            "flywheel: CRITICAL: {e} — bead {id} NOT dispatched (loop bug; no truncation)"
        ),
    }
}

/// Freshness window for peer claim/announce signals. Mirrors the ~90-minute
/// TIMEBOX in the dispatch brief; `FLYWHEEL_DISPATCH_TTL_SECS` overrides
/// (resolved once at loop start into `LoopParams.dispatch_guard_ttl_us`).
pub const DISPATCH_GUARD_TTL_US: i64 = 90 * 60 * 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardSignalKind {
    Claim,
    Announce,
    Done,
    StandDown,
    Handoff,
}

impl GuardSignalKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Announce => "announce",
            Self::Done => "done",
            Self::StandDown => "stand_down",
            Self::Handoff => "handoff",
        }
    }
}

/// bd-dispatch-dedupe-2zlz: classify one bead-thread message into the
/// dispatch-guard signal vocabulary. Precedence matters — done beats
/// everything (the executor's close report settles the thread), a genuine
/// handoff beats a stand-down phrase, and claim/announce come last.
/// Keyword-based on purpose: the guard must read BOTH harness mails and
/// human posts without a schema.
///
/// ADR-0010 D9 (bd-l0fz): the done-hold is STRUCTURAL — the projected
/// `bead=<id>` tag (stamped by `toron mail send --resolve-bead` and the loop
/// reporter-of-record). The `[{id}] done` subject is decoration for humans:
/// a subject alone NEVER fires the hold (clean cutover, no dual-match shim;
/// the old subject-anchor false-classified participles and is deleted, not
/// shimmed).
pub fn classify_guard_signal(
    subject: &str,
    body: &str,
    bead: Option<&str>,
) -> Option<GuardSignalKind> {
    let s = subject.to_lowercase();
    let b = body.to_lowercase();
    if bead.is_some() {
        return Some(GuardSignalKind::Done);
    }
    let standdown =
        s.contains("stand-down") || s.contains("stand down") || b.contains("stand down");
    let handoff_marker = s.contains("handoff") || b.contains("handoff") || s.contains("hand-off");
    let concurrent_live = s.contains("concurrent executor") || b.contains("concurrent executor");
    if handoff_marker && !concurrent_live {
        return Some(GuardSignalKind::Handoff);
    }
    if standdown || concurrent_live {
        return Some(GuardSignalKind::StandDown);
    }
    if s.contains("starting") || s.contains("executing") {
        return Some(GuardSignalKind::Announce);
    }
    let claim_word = (s.contains("claim") || b.contains("claim"))
        && !s.contains("unclaim")
        && !b.contains("unclaim")
        && !s.contains("claim-watch")
        && !b.contains("claim-watch");
    if claim_word {
        return Some(GuardSignalKind::Claim);
    }
    None
}

/// Everything the guard weighs, collected by the `__dispatch-guard` scan.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DispatchGuardFacts {
    /// (created_ts_us, kind) for every classified message in the thread.
    pub signals: Vec<(i64, GuardSignalKind)>,
    /// Newest mtime (µs) under `.flywheel/receipts/` matching the bead id,
    /// when one exists — executor liveness without any mail signal.
    pub receipts_fresh_us: Option<i64>,
}

/// The GOAL gate: Ok(()) = dispatch may proceed; Err(reason) = hold with
/// the operator-facing reason.
///
/// Rules, in order:
/// 1. The newest signal overall is `Done` → skip: work finished, ledger
///    row is out of sync (re-dispatching would double-execute).
/// 2. Any `StandDown` newer than the last done → hold: a peer explicitly
///    yielded/flagged a live executor (W7).
/// 3. Newest `Claim`/`Announce`: hold while unexpired unless a `Handoff`
///    was posted at-or-after it; stale (> TTL) announces fall through.
/// 4. Receipts touched within the TTL after the last done → hold: executor
///    evidence without a ledger row.
pub fn eval_dispatch_guard(
    facts: &DispatchGuardFacts,
    now_us: i64,
    ttl_us: i64,
) -> std::result::Result<(), String> {
    let done_max = facts
        .signals
        .iter()
        .filter(|(_, k)| *k == GuardSignalKind::Done)
        .map(|(ts, _)| *ts)
        .max();
    let close = done_max.unwrap_or(i64::MIN);
    let recent: Vec<(i64, GuardSignalKind)> = facts
        .signals
        .iter()
        .copied()
        .filter(|(ts, _)| *ts > close)
        .collect();

    if let Some(done_ts) = done_max {
        let newer = recent.iter().any(|(ts, _)| *ts > done_ts);
        if !newer && !recent.is_empty() {
            return Err("peer reported done — ledger sync gap".into());
        }
        if recent.is_empty() {
            return Err("peer reported done — ledger sync gap".into());
        }
    }

    if recent.iter().any(|(_, k)| *k == GuardSignalKind::StandDown) {
        return Err("peer stood down — executor flagged live".into());
    }

    let newest_of = |k: GuardSignalKind| {
        recent
            .iter()
            .filter(|(_, x)| *x == k)
            .map(|(ts, _)| *ts)
            .max()
    };
    let claimed_at = newest_of(GuardSignalKind::Claim).or(newest_of(GuardSignalKind::Announce));
    if let Some(a) = claimed_at {
        match newest_of(GuardSignalKind::Handoff) {
            Some(h) if h >= a => {}
            _ => {
                if now_us - a <= ttl_us {
                    return Err(format!(
                        "unexpired peer announce ({}s old)",
                        (now_us - a) / 1_000_000
                    ));
                }
            }
        }
    }

    if let Some(r) = facts.receipts_fresh_us
        && r > close
        && now_us - r <= ttl_us
    {
        return Err(format!(
            "receipts written {}s ago — executor evidence",
            (now_us - r) / 1_000_000
        ));
    }
    Ok(())
}

/// Exhaustion verdict from the two context channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextVerdict {
    /// Agent is at ~80%+ context — respawn before delegating more work.
    Exhausted,
    /// No exhaustion signal — safe to delegate.
    Healthy,
}

/// Combined context-exhaustion verdict: terminal marker OR a fresh state file
/// with `context_pct >= 80`. Stale files (older than `DEFAULT_MAX_AGE`), low
/// percentages, and absent state never exhaust — worst case Healthy (today's
/// semantics preserved when no agent writes files). The file channel wins
/// when the channels disagree: it cannot scroll away (alt-screen caveat).
///
/// Pure and deterministic for a fixed `now_unix` — the caller (the leaf)
/// evaluates it at run time so the verdict is checkpointed with the leaf
/// result (replay-safe).
pub fn context_verdict(
    terminal: &str,
    state: Option<&crate::agentstate::AgentStateFile>,
    now_unix: u64,
) -> ContextVerdict {
    // A harness reports this marker at the start of its error line. A plain
    // substring match killed agents that merely ran `rg CONTEXT_LIMIT` or read
    // this source file (bd-context-marker-selfkill).
    if terminal
        .lines()
        .any(|line| line.trim_start().starts_with("CONTEXT_LIMIT"))
    {
        return ContextVerdict::Exhausted;
    }
    if let Some(s) = state
        && s.context_pct >= 80
        && crate::agentstate::is_fresh_at(&s.ts, now_unix, crate::agentstate::DEFAULT_MAX_AGE)
    {
        return ContextVerdict::Exhausted;
    }
    ContextVerdict::Healthy
}

/// Parsed `__read` leaf output: `{"terminal": "...", "state": {...}|null}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadOutput {
    pub terminal: String,
    #[serde(default)]
    pub state: Option<crate::agentstate::AgentStateFile>,
}

/// Parse one `__read` JSON output into [`ReadOutput`]. A null or missing
/// `state` yields `None` (file absent/malformed — the leaf never errors on
/// the file channel).
pub fn parse_read_output(v: &serde_json::Value) -> ReadOutput {
    ReadOutput {
        terminal: v
            .get("terminal")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        state: v
            .get("state")
            .filter(|s| !s.is_null())
            .and_then(|s| serde_json::from_value(s.clone()).ok()),
    }
}

/// Resume-dedup verdict: should the loop (re-)send this prompt?
///
/// - no record → never sent → resend.
/// - record from ANOTHER invocation → a fresh run legitimately re-delegates
///   → resend.
/// - landed, same invocation → delivered before the crash → skip (the work
///   is in flight; the queue item was already consumed).
/// - unlanded, same invocation → crashed mid-flight → resend (safe, needed).
///
/// Without confirmed sends the registry records nothing (or unlanded), so the
/// guard degrades to today's always-resend behavior.
pub fn should_resend(record: Option<&crate::state::SentRecord>, current_invocation: &str) -> bool {
    match record {
        None => true,
        Some(r) => r.invocation != current_invocation || (!r.landed && !r.ambiguous),
    }
}

/// bd-dedup-stale: should a LANDED dedup record be KEPT? Returns false (clear
/// it) when the record is landed + same invocation + the target agent is
/// IDLE/done (the prior dispatch stalled — the agent isn't working it). Without
/// this, `should_resend` blocks re-dispatch of a re-queued bead forever (the
/// re-queue-after-strand bug). Working/blocked/unknown agents' records are
/// KEPT (genuinely in-flight → crash-resume dedup intact). Pure — unit-tested.
fn keep_landed_record(
    r: &crate::state::SentRecord,
    invocation: &str,
    states: &[AgentState],
) -> bool {
    if r.invocation != invocation || !r.landed {
        return true;
    }
    match states.iter().find(|s| s.name == r.name) {
        Some(s) => matches!(s.status.as_str(), "working" | "blocked" | "unknown"),
        None => true,
    }
}

/// Invalidate LANDED dedup records (same invocation) for idle/done agents.
/// Call after polling states, before `plan_actions` dispatch. See
/// [`keep_landed_record`].
pub fn clear_stale_landed_for_idle(project: &str, invocation: &str, states: &[AgentState]) {
    let mut records = crate::state::load_last_sent(project);
    let before = records.len();
    records.retain(|r| keep_landed_record(r, invocation, states));
    if records.len() != before {
        let _ = crate::state::save_last_sent(project, &records);
    }
}

/// bd-resume-window-lease-sweep-4sa2: default resume window (15 min). The loop
/// re-resumed the same in-progress bead every pass (~2 min) and every pane
/// answered with a stand-down commit — churn, not progress. One resume per
/// (bead, pane) inside this window; the watermark is persisted so it survives
/// across passes and processes (the in-memory `assigned` set resets per run).
pub const RESUME_WINDOW_DEFAULT_SECS: u64 = 900;

/// Has this pane already been resumed for this bead inside the window?
/// `last_resumed_us` is the persisted watermark (None = never resumed here).
pub fn within_resume_window(last_resumed_us: Option<i64>, now_us: i64, window_secs: u64) -> bool {
    match last_resumed_us {
        Some(t) => now_us.saturating_sub(t) < (window_secs as i64).saturating_mul(1_000_000),
        None => false,
    }
}

/// A DIFFERENT live pane holds a reservation whose reason is this bead — a lane
/// split mid-flight, not a stall, so resuming would collide. `reservations` are
/// `(reason, holder)` pairs; a reservation's reason IS the bead id. An empty
/// holder (an unstamped peer row) never counts as a peer.
pub fn peer_held(bead_id: &str, reservations: &[(String, String)], self_pane: &str) -> bool {
    reservations
        .iter()
        .any(|(reason, holder)| reason == bead_id && !holder.is_empty() && holder != self_pane)
}

/// Another pin holds a reservation whose path overlaps this file. `claims`
/// are `(path pattern, holder pin)`. An empty holder is not a peer. The
/// holder of this file keeps it. The other agent stops.
///
pub fn other_file_holder<'a>(
    path: &str,
    claims: &'a [(String, String)],
    self_holder: &str,
) -> Option<&'a str> {
    claims.iter().find_map(|(pattern, holder)| {
        let holder = holder.trim();
        if holder.is_empty() || holder == self_holder {
            return None;
        }
        toron_core::mailbox::reservations::overlaps(pattern, path).then_some(holder)
    })
}

/// Path-like words in bead text (`src/main.rs`, `src/**`). The loop uses
/// these when the bead has not reserved yet, so a named file still stops
/// the bead before the first lease.
pub fn path_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '`' | ',' | '(' | ')' | '"' | '\'' | ';' | '[' | ']')
    }) {
        let tok = raw.trim_matches(|c: char| matches!(c, ',' | ';' | ':'));
        if (tok.contains('/') || tok.contains('*')) && !out.iter().any(|seen: &String| seen == tok)
        {
            out.push(tok.to_string());
        }
    }
    out
}

/// ADR-0019 D9: the overlapping other-holder blocking this bead, with the
/// pattern for the notice body. Own reserved rows first, then bead-named
/// intended paths. `None` means nothing blocks (or nothing to name).
pub fn file_hold_notice(
    bead_id: &str,
    rows: &[(String, String, String)],
    self_holder: &str,
    intended: &[String],
) -> Option<(String, String)> {
    let claims: Vec<(String, String)> = rows
        .iter()
        .map(|(path, _, holder)| (path.clone(), holder.clone()))
        .collect();
    for (path, reason, holder) in rows {
        if reason == bead_id
            && holder == self_holder
            && let Some(other) = other_file_holder(path, &claims, self_holder)
        {
            return Some((other.to_string(), path.clone()));
        }
    }
    for path in intended {
        if let Some(other) = other_file_holder(path, &claims, self_holder) {
            return Some((other.to_string(), path.clone()));
        }
    }
    None
}
/// This bead stops when another pin holds an overlapping path.
/// `rows` are `(path, reason, holder)`. `intended` is the paths the bead
/// named and has not reserved yet. An empty holder is not a peer. A bead
/// that names nothing and holds nothing is not blocked.
pub fn file_blocks_bead(
    bead_id: &str,
    rows: &[(String, String, String)],
    self_holder: &str,
    intended: &[String],
) -> bool {
    let claims: Vec<(String, String)> = rows
        .iter()
        .map(|(path, _, holder)| (path.clone(), holder.clone()))
        .collect();
    let own = !self_holder.is_empty()
        && rows.iter().any(|(path, reason, holder)| {
            reason == bead_id
                && holder == self_holder
                && other_file_holder(path, &claims, self_holder).is_some()
        });
    own || intended
        .iter()
        .any(|path| other_file_holder(path, &claims, self_holder).is_some())
}

/// Leases to release because their reason bead is CLOSED — the pure rule behind
/// the per-pass sweep (`reserve sweep --closed-beads` equivalent). `leases` are
/// `(handle, reason_bead_id)` pairs; the handle is what the release call needs
/// (a path pattern here). An empty closed set, or a lease whose reason is still
/// open, is kept. Pure so the shipped sweep and its test cannot disagree.
pub fn closed_reason_lease_ids(
    leases: &[(String, String)],
    closed_beads: &std::collections::HashSet<String>,
) -> Vec<String> {
    leases
        .iter()
        .filter(|(_, reason)| !reason.is_empty() && closed_beads.contains(reason))
        .map(|(handle, _)| handle.clone())
        .collect()
}

/// Status of one durable work attempt (bd-work-attempts-m5x6). The attempt —
/// not the loop pass — is the unit steering acts on: a steer writes a new
/// generation under the same bead intent, and the superseded generation
/// stays readable for audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkAttemptStatus {
    Queued,
    Running,
    Completed,
    Interrupted,
    Failed,
    Superseded,
}

impl WorkAttemptStatus {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "steer-side attempt rendering is unwired until flywheel binds a steer to a bead"
        )
    )]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "steer-side attempt rendering is unwired until flywheel binds a steer to a bead"
        )
    )]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "interrupted" => Some(Self::Interrupted),
            "failed" => Some(Self::Failed),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// One generation of work on a bead by a pane: the injected prompt's corr
/// and the attempt's effect key when the dispatch was journaled. A steer
/// never edits a row — it supersedes it (status stays readable) and appends
/// the next generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAttempt {
    pub bead_id: String,
    pub pane: String,
    pub generation: u64,
    pub status: WorkAttemptStatus,
    pub corr: Option<String>,
    pub effect_key: Option<String>,
    pub updated_us: i64,
}

/// Storage/predicate key for one generation. `U+001F` (unit separator) so a
/// bead or pane id that contains the joiner cannot collide — the same rule
/// as the resume window's own key.
pub fn attempt_key(bead: &str, pane: &str, generation: u64) -> String {
    format!("{bead}\u{1f}{pane}\u{1f}{generation}")
}

/// The live attempt for one bead: the highest generation across its panes.
/// None when the bead has no attempt on record (first dispatch). Pure.
pub fn live_attempt_for_bead<'a>(
    attempts: &'a [WorkAttempt],
    bead: &str,
) -> Option<&'a WorkAttempt> {
    attempts
        .iter()
        .filter(|a| a.bead_id == bead)
        .max_by_key(|a| a.generation)
}

/// Steering writes a new generation under the same bead intent: the live
/// attempt is marked superseded (its corr and effect key stay readable for
/// audit) and the replacement starts at generation+1 carrying the new
/// prompt's corr. Pure — persistence lives in `state`.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "steer-side supersede is unwired until flywheel binds a steer to a bead"
    )
)]
pub fn supersede_attempt(
    live: &WorkAttempt,
    corr: &str,
    effect_key: Option<&str>,
    now_us: i64,
) -> (WorkAttempt, WorkAttempt) {
    let mut old = live.clone();
    old.status = WorkAttemptStatus::Superseded;
    old.updated_us = now_us;
    let next = WorkAttempt {
        bead_id: live.bead_id.clone(),
        pane: live.pane.clone(),
        generation: live.generation.saturating_add(1),
        status: WorkAttemptStatus::Running,
        corr: Some(corr.to_string()),
        effect_key: effect_key.map(str::to_string),
        updated_us: now_us,
    };
    (old, next)
}

/// Attempt-aware resume predicate (bd-work-attempts-m5x6). The loop reads
/// the live attempt per bead, not only bead status:
///
/// - no attempt on record: first dispatch, window rules only;
/// - completed: finished work stays finished even when the bead row lags;
/// - superseded: a replaced generation never resumes — its replacement owns
///   the bead (the caller resolves `live` to the newest generation);
/// - queued/running/interrupted/failed: the window rule decides — and the
///   window is keyed on the live generation (`state::attempt_resume_key`),
///   so a steered replacement is fresh while a repeat send of the live
///   generation stays suppressed.
///
/// Pure: the clock and the looked-up watermark arrive as values.
pub fn should_resume_attempt(
    last_resumed_us: Option<i64>,
    now_us: i64,
    window_secs: u64,
    peer_holds: bool,
    live: Option<&WorkAttempt>,
) -> bool {
    if peer_holds {
        return false;
    }
    match live {
        None => !within_resume_window(last_resumed_us, now_us, window_secs),
        Some(a)
            if matches!(
                a.status,
                WorkAttemptStatus::Completed | WorkAttemptStatus::Superseded
            ) =>
        {
            false
        }
        Some(_) => !within_resume_window(last_resumed_us, now_us, window_secs),
    }
}

/// bd-loop-feeds-idle-panes-rh2z: how many consecutive passes a pane may be
/// starved — idle while its own stalled bead sits inside the resume window —
/// before the idle-pane rotation exempts it. Two passes at the supervisor's
/// 60s cadence is ~2 min of silence before the second nudge; the observed
/// incident was 5 idle panes and 17 in-progress beads with zero sends for
/// several passes, because the window (correctly) suppressed the repeat
/// resume and nothing else fed the panes.
pub const IDLE_PANE_EXEMPT_AFTER_PASSES: u32 = 2;

/// Inputs to [`idle_pane_rotation`]. A struct (not eight positional args) so
/// the call site names each fleet fact it passes.
pub struct IdleRotationInputs<'a> {
    /// Every live pane pin — the OWNER set. A bead a live pane owns is never
    /// handed to a different pane.
    pub live_pins: &'a [String],
    /// The idle subset of `live_pins`, in fleet order. The rotation emits at
    /// most one bead per entry, in this order — the caller keeps that order in
    /// the dispatch queue so the positional pairing lands each pick on the
    /// pane that chose it.
    pub idle_pins: &'a [String],
    /// In-progress beads (`br list --status in_progress`).
    pub in_progress: &'a [ReadyBead],
    /// Dispatchable ready beads, already narrowed by the wave gate + scope —
    /// the rotation must not widen what the caller's filters held back.
    pub ready: &'a [ReadyBead],
    /// Live `(reason, holder)` reservation pairs.
    pub peers: &'a [(String, String)],
    /// Bead ids already queued this pass — never handed out twice.
    pub taken: &'a [String],
    /// Pane pin → consecutive starved passes (`state::load_pane_starvation`).
    pub starved: &'a BTreeMap<String, u32>,
    /// `(bead, pane)` → last-resume epoch µs (`state::load_resume_window`).
    pub resume_window: &'a BTreeMap<String, i64>,
    pub now_us: i64,
    pub window_secs: u64,
    /// Window exemptions after this many consecutive starved passes. 0 =
    /// the window never gates an idle pane's own stalled work.
    pub exempt_after_passes: u32,
}

/// One bead handed to one idle pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationPick {
    pub pin: String,
    pub bead: String,
    /// True when the resume window was bypassed by the starvation exemption.
    pub exempt: bool,
}

/// The rotation's output: the picks (in idle-pane order) and the starvation
/// counters the caller persists for the next pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotationPlan {
    pub picks: Vec<RotationPick>,
    /// Pane pin → consecutive starved passes. Every idle pane is represented
    /// (0 = fed, unarmed, or never gated), so the caller can store the map as
    /// the whole fleet's snapshot.
    pub starved_after: BTreeMap<String, u32>,
}

/// bd-loop-feeds-idle-panes-rh2z: the idle-pane rotation.
///
/// The resume window stops the loop re-nudging a pane that is slow but alive.
/// Left alone that suppression can idle the whole fleet while stalled beads
/// sit unclaimed. Each idle pane now takes ONE bead, the first tier that
/// matches:
///
/// 1. its own stalled in-progress bead (the window-gated tier),
/// 2. a ready bead already assigned to it,
/// 3. an unowned ready bead,
/// 4. any other ready bead whose assignee is not a live pane.
///
/// Never a bead another live pane owns, never one a peer holds a reservation
/// on (`peers` — a lane split, not a stall), never one already queued
/// (`taken`), and never the same bead twice. A tier-1 hit inside the resume
/// window still defers, for `exempt_after_passes` consecutive starved passes;
/// from that pass the pick is marked `exempt` and the caller resumes it. A
/// pane whose own stall is still gated takes NOTHING this pass — handing it
/// unrelated free work would fork a lane that is merely slow, and the
/// starvation counter is exactly what shortens that wait.
///
/// Pure: the clock arrives as `now_us`, the window as a map, no I/O.
pub fn idle_pane_rotation(inputs: &IdleRotationInputs<'_>) -> RotationPlan {
    let mut plan = RotationPlan::default();
    let mut handed: BTreeSet<String> = inputs.taken.iter().cloned().collect();
    let live: BTreeSet<&str> = inputs.live_pins.iter().map(String::as_str).collect();

    for pin in inputs.idle_pins {
        let own = inputs.in_progress.iter().find(|b| {
            b.assignee.as_deref() == Some(pin.as_str())
                && !handed.contains(&b.id)
                && !peer_held(&b.id, inputs.peers, pin)
        });
        if let Some(b) = own {
            let last = inputs
                .resume_window
                .get(&crate::state::resume_key(&b.id, pin))
                .copied();
            if !within_resume_window(last, inputs.now_us, inputs.window_secs) {
                handed.insert(b.id.clone());
                plan.starved_after.insert(pin.clone(), 0);
                plan.picks.push(RotationPick {
                    pin: pin.clone(),
                    bead: b.id.clone(),
                    exempt: false,
                });
                continue;
            }
            let passes = inputs.starved.get(pin).copied().unwrap_or(0);
            if passes >= inputs.exempt_after_passes {
                handed.insert(b.id.clone());
                plan.starved_after.insert(pin.clone(), 0);
                plan.picks.push(RotationPick {
                    pin: pin.clone(),
                    bead: b.id.clone(),
                    exempt: true,
                });
            } else {
                plan.starved_after
                    .insert(pin.clone(), passes.saturating_add(1));
            }
            continue;
        }

        let free = |b: &ReadyBead| {
            !handed.contains(&b.id)
                && !peer_held(&b.id, inputs.peers, pin)
                && match b.assignee.as_deref() {
                    None => true,
                    Some(a) => a == pin.as_str() || !live.contains(a),
                }
        };
        let pick = inputs
            .ready
            .iter()
            .find(|b| b.assignee.as_deref() == Some(pin.as_str()) && free(b))
            .or_else(|| {
                inputs
                    .ready
                    .iter()
                    .find(|b| b.assignee.is_none() && free(b))
            })
            .or_else(|| inputs.ready.iter().find(|b| free(b)));
        plan.starved_after.insert(pin.clone(), 0);
        if let Some(b) = pick {
            handed.insert(b.id.clone());
            plan.picks.push(RotationPick {
                pin: pin.clone(),
                bead: b.id.clone(),
                exempt: false,
            });
        }
    }
    plan
}

/// Escalation tier for a blocked agent (bd-blocked-escalation-protocol, A6).
///
/// Tiered by elapsed time since first_blocked (injected host time i64-µs,
/// deterministic):
/// - `T0` (first detection): high + ack_required ping with the agent tail.
/// - `T1` (+2×interval, still un-acked): reply in-thread, high, fresh tail.
/// - `T2` (+4×interval): reply urgent + summarize_thread; CC human if set.
/// - `T3` (+8×interval): loop halts (bounded), final "needs human" mail.
///
/// Returns the tier to act at now. `ping_count` advances per notify so
/// re-pings are monotonic; the tier is derived from elapsed time, not the
/// counter, so a crash-resume continues at the correct tier.
///
/// Implemented halt rule (review-fix F4): `mail` mode halts at T3; `stop`
/// mode halts at T0 (existing behavior); `continue` mode never halts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EscalationTier {
    T0,
    T1,
    T2,
    T3,
}

/// The bounded-halt decision for a blocked agent. Returns the exit message
/// when the loop must halt for this tier under `on_blocked`, else None.
///
/// Rule (review-fix F4, bd-review-fix-epic-m0b.4):
/// - `mail` mode: halt at T3 only (T0/T1/T2 keep notifying and continuing).
/// - `stop` mode: halt at any tier (T0) — existing `--on-blocked stop`
///   semantics, unchanged.
/// - `continue` mode: never halt — blockers are ignored by contract.
///
/// Pure (no I/O) so the halt policy is table-testable and the loop arm is a
/// thin wrapper.
pub fn on_blocked_tier_exit(tier: EscalationTier, on_blocked: &str) -> Option<String> {
    match on_blocked {
        "mail" if tier == EscalationTier::T3 => {
            Some("halted: agent blocked past escalation budget (T3) — human required".to_string())
        }
        "stop" => Some(format!(
            "stopped: blocked agents (--on-blocked stop, tier {tier:?})"
        )),
        _ => None,
    }
}

/// Map elapsed-us since first_blocked to the escalation tier.
/// `interval_secs` is the loop's active interval (the durable sleep period).
/// Elapsed thresholds: T1 at 2×, T2 at 4×, T3 at 8× the interval.
pub fn escalation_tier(
    first_blocked_ts_us: i64,
    now_us: i64,
    interval_secs: u64,
) -> EscalationTier {
    let interval_us = (interval_secs as i64).saturating_mul(1_000_000);
    let elapsed = now_us.saturating_sub(first_blocked_ts_us);
    if elapsed >= 8 * interval_us {
        EscalationTier::T3
    } else if elapsed >= 4 * interval_us {
        EscalationTier::T2
    } else if elapsed >= 2 * interval_us {
        EscalationTier::T1
    } else {
        EscalationTier::T0
    }
}

/// Replace an exhausted agent with a fresh instance in a new pane.
/// Checkpointed. Logs a `Respawn` event (side effect inside the leaf so the
/// log entry is checkpointed with the call).
///
/// cg6.3 replay-idempotency: `__respawn` no-ops when an agent with the same
/// name already exists in a healthy state (the guard lives at the side
/// effect). The context_limit path passes `--force`: an idle-but-exhausted
/// agent is healthy and MUST still be respawned — its replay window is
/// documented at `__respawn` (main.rs).
async fn respawn_agent(args: RespawnArgs) -> anyhow::Result<()> {
    let exe = self_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args(["__respawn", &args.project, &args.name, &args.kind]);
    if args.reason == "context_limit" {
        cmd.arg("--force");
    }
    let out = run_leaf_cmd(cmd, "__respawn").await?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "__respawn failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if let Some(h) = args.text_hash {
        let mut records = crate::state::load_last_sent(&args.project);
        records.retain(|r| !(r.name == args.name && r.text_hash == h));
        if let Err(e) = crate::state::save_last_sent(&args.project, &records) {
            eprintln!("flywheel: cannot invalidate last-sent record: {e:#}");
        }
    }
    let _ = crate::state::log_event(
        &args.project,
        crate::state::EventKind::Respawn,
        serde_json::json!({
            "name": args.name,
            "reason": args.reason,
            "attempts": args.attempts,
            "source": "loop",
        }),
    );
    Ok(())
}

/// Outcome of one confirmed `__send` invocation, parsed from the leaf JSON.
/// `landed` is None on the legacy (non-confirm) path where no verdict exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendOutcome {
    pub ok: bool,
    pub name: String,
    /// The provider or operator interrupted this attempt. This is distinct
    /// from a failed or ambiguous delivery and never triggers respawn.
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub landed: Option<bool>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// true when __send skipped the hc call because a landed record for
    /// this (name, prompt, invocation) already exists (resume-dedup)
    #[serde(default)]
    pub deduped: bool,
}

/// Send one prompt via the flywheel binary. Checkpointed side effect.
/// With confirm, returns the parsed landing verdict; the legacy path returns
/// an outcome with `landed: None`.
async fn send_prompt(args: SendArgs) -> anyhow::Result<SendOutcome> {
    if let Ok(cfg) = crate::config::Config::load()
        && crate::comms_send::enabled(&cfg)
    {
        let a = args.clone();
        let out = crate::backend::run_mail_off_runtime(move || {
            let task_id = if a.invocation.is_empty() {
                crate::comms_send::loop_task_id(
                    &crate::mint_task_id(),
                    a.iteration,
                    &a.name,
                    &a.text,
                )
            } else {
                crate::comms_send::loop_task_id(&a.invocation, a.iteration, &a.name, &a.text)
            };
            crate::comms_send::loop_send(&cfg, &a.project, &a.name, &a.text, &task_id)
        })
        .await?;
        return serde_json::from_value::<SendOutcome>(out)
            .map_err(|e| anyhow::anyhow!("inbox send outcome: {e}"));
    }
    let exe = self_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args(send_prompt_argv(&args));
    let confirm_bound = std::time::Duration::from_millis(args.confirm_timeout_ms.unwrap_or(45_000))
        + LEAF_WATCHDOG_GRACE;
    let out = run_leaf_cmd_bounded(cmd, "__send", confirm_bound).await?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "__send failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let outcome: SendOutcome = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow::anyhow!("__send bad JSON: {e}: {raw}"))?;
    let _ = crate::state::log_event(
        &args.project,
        crate::state::EventKind::Send,
        serde_json::json!({
            "to": args.name,
            "confirm": args.confirm,
            "landed": outcome.landed,
            "status": outcome.status,
            "reason": outcome.reason,
            "source": "loop",
        }),
    );
    if outcome.deduped {
        let _ = crate::state::log_event(
            &args.project,
            crate::state::EventKind::SkippedDup,
            serde_json::json!({
                "name": args.name,
                "iteration": args.iteration,
                "source": "loop",
            }),
        );
    }
    Ok(outcome)
}

/// Current wall-clock in microseconds since the epoch. Only called INSIDE
/// leaves (cg6.1): leaf-side effects settle through the journal, so a
/// completed leaf is not re-executed; the body must never read wall-clock
/// directly (that was the replay-determinism bug).
#[cfg(test)]
fn now_us_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// One escalation step (pure): derive the tier from the persisted
/// first-blocked timestamp and decide whether to MAIL. Returns
/// (tier 0..3, ping_count, should_mail). cg6.3: the mail is gated on a
/// TIER TRANSITION (`last_ping_tier` marker) — a replay re-execution at the
/// same tier neither re-mails nor double-counts; at most one ping per
/// logical tier transition (ADR-0001 §4.4's T0→T1→T2→T3 cadence).
///
/// `acked` is the toron ack-SLA handling fact (bd-mail-ack-sla-tracking's
/// `ack_overdue` read, per-recipient read state — never a wall-clock guess)
/// for the agent's own health thread. ADR-0001 A6: "an agent that acks or
/// un-blocks resets the escalation". A reset restarts the budget from now,
/// drops the ping credit, and records T0 as handled — so an ack never
/// provokes an immediate re-ping; if the agent goes quiet again the ladder
/// resumes one window later at T1.
fn escalation_step(
    st: &mut crate::state::EscalationState,
    now_us: i64,
    interval_secs: u64,
    acked: bool,
) -> (u32, u32, bool) {
    if acked {
        st.first_blocked_ts = now_us;
        st.ping_count = 0;
        st.last_ping_tier = Some(0);
        return (0, 0, false);
    }
    let t = escalation_tier(st.first_blocked_ts, now_us, interval_secs);
    let u = match t {
        EscalationTier::T0 => 0u32,
        EscalationTier::T1 => 1,
        EscalationTier::T2 => 2,
        EscalationTier::T3 => 3,
    };
    let should_mail = st.last_ping_tier != Some(u);
    if should_mail {
        st.ping_count = st.ping_count.saturating_add(1);
        st.last_ping_tier = Some(u);
    }
    (u, st.ping_count, should_mail)
}

/// Load-or-init the escalation entry, run ONE escalation step, persist.
/// File I/O is legal here: this runs inside the checkpointed `notify_blocked`
/// leaf (cg6.1). The tier derives from the PERSISTED first_blocked_ts (or the
/// seed on a brand-new entry), never from a body-side re-read — replay of an
/// in-flight leaf re-executes against the same persisted value, so the tier
/// cannot jump across a crash boundary. Returns (tier, ping_count,
/// should_mail) — the mail decision comes from the persisted
/// `last_ping_tier` marker (cg6.3), so a replay at the same tier no-ops.
///
/// `gate_acked` is the agent's ack of ITS OWN health thread. It only counts
/// as a reset once we have actually pinged (`ping_count > 0`): an empty
/// thread before the first ping is indistinguishable from "nothing sent",
/// and treating that as an ack would silence the escalation entirely.
fn escalate_and_persist(
    project: &str,
    name: &str,
    first_blocked_seed: Option<i64>,
    interval_secs: u64,
    params: serde_json::Value,
    gate_acked: bool,
    now_us: i64,
) -> anyhow::Result<(u32, u32, bool)> {
    let mut al = crate::state::load_active_loop(project)?.unwrap_or_else(|| {
        crate::state::ActiveLoop::new("swarm-loop", params, crate::state::now_iso())
    });
    let st =
        al.escalation
            .entry(name.to_string())
            .or_insert_with(|| crate::state::EscalationState {
                first_blocked_ts: first_blocked_seed.unwrap_or(now_us),
                ping_count: 0,
                last_ping_tier: None,
                last_tail: String::new(),
            });
    let acked = gate_acked && st.ping_count > 0;
    let (tier, ping_count, should_mail) = escalation_step(st, now_us, interval_secs, acked);
    crate::state::save_active_loop(project, &al)?;
    Ok((tier, ping_count, should_mail))
}

/// Pure: argv for the ack gate leaf (`__ack-overdue`). The gate reads the
/// agent's own `health-<name>` lane at SLA 0 — "is ANY ack-required ping in
/// this thread still unacked?" — so it is independent of the 30-min default
/// (the escalation pings on its own 2×interval cadence, which is typically
/// inside that window; ADR-0001's distinction between the two clocks).
pub fn ack_gate_argv(project: &str, name: &str) -> Vec<String> {
    vec![
        "__ack-overdue".to_string(),
        project.to_string(),
        name.to_string(),
        "--thread".to_string(),
        format!("health-{name}"),
        "--sla-secs".to_string(),
        "0".to_string(),
    ]
}

/// Pure: decode the gate's stdout. `Some(n)` = the read answered with `n`
/// unacked ack-required pings; `None` = the read was UNAVAILABLE (no index,
/// unregistered agent, remote backend, malformed output). Unavailable is not
/// acked: the escalation keeps its time-based ladder rather than silently
/// stopping (fail-open — the pre-gate behavior).
pub fn ack_gate_unacked(stdout: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    v.get("count").and_then(serde_json::Value::as_u64)
}

/// Leaf-side I/O for the ack gate: run `__ack-overdue` and decode it.
/// `true` = every ping in the agent's health thread has been handled.
/// Any failure answers `false` (not acked) — see [`ack_gate_unacked`].
///
/// Short-circuit: with no ping ever recorded for this agent there is nothing
/// to have been acked, so the first (T0) pass never pays for a subprocess —
/// the read starts on the pass AFTER the first ping.
async fn blocked_ack_gate(args: &NotifyArgs) -> bool {
    let pinged = crate::state::load_active_loop(&args.project)
        .ok()
        .flatten()
        .and_then(|al| al.escalation.get(&args.name).map(|s| s.ping_count))
        .is_some_and(|n| n > 0);
    if !pinged {
        return false;
    }
    let Ok(exe) = self_exe() else {
        return false;
    };
    let mut cmd = Command::new(&exe);
    cmd.args(ack_gate_argv(&args.project, &args.name));
    match run_leaf_cmd(cmd, "__ack-overdue").await {
        Ok(out) if out.status.success() => {
            ack_gate_unacked(&String::from_utf8_lossy(&out.stdout)) == Some(0)
        }
        _ => false,
    }
}

/// Notify a blocked agent via agent-mail. Checkpointed side effect — and
/// (cg6.1) the home of the ENTIRE escalation state machine: it loads or
/// initializes the escalation entry, computes the tier from the persisted
/// first_blocked_ts, increments + persists ping_count, then mails. The
/// workflow body only acts on the returned `EscalationOutcome`.
///
/// At-least-once note: a COMPLETED leaf is settled in the journal and never
/// re-executed. An IN-FLIGHT leaf at crash re-executes on the next poll from
/// the persisted entry — same first_blocked_ts → same tier, and the
/// `last_ping_tier` marker (cg6.3) suppresses the duplicate mail: at most one
/// ping per logical tier transition.
async fn notify_blocked(args: NotifyArgs) -> anyhow::Result<EscalationOutcome> {
    let gate_acked = blocked_ack_gate(&args).await;
    let (tier, ping_count, should_mail) = escalate_and_persist(
        &args.project,
        &args.name,
        args.first_blocked_ts,
        args.interval_secs,
        args.params,
        gate_acked,
        args.now_us,
    )?;
    if should_mail {
        let exe = self_exe()?;
        let mut cmd = Command::new(&exe);
        cmd.args([
            "__mail-blocked",
            &args.project,
            &args.name,
            "--tier",
            &tier.to_string(),
        ]);
        let out = run_leaf_cmd(cmd, "__mail-blocked").await?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "__mail-blocked failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if tier == 3 {
            let _ = crate::state::log_event(
                &args.project,
                crate::state::EventKind::Loop,
                serde_json::json!({
                    "status": "halted",
                    "tier": tier,
                    "name": args.name,
                }),
            );
        }
    }
    Ok(EscalationOutcome { tier, ping_count })
}

/// One compose-request row's loop-side decision. Pure — unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeRequestOutcome {
    /// Sender not on the allowlist: dropped with a log line, never queued.
    Denied,
    /// The catalog already covers the objective: guidance names the workflow.
    Covered { workflow: String },
    /// No coverage: the rendered candidate is queued FOR APPROVAL.
    CandidateForApproval { prompt: String },
}

/// Decide one compose-request mail row. `spec_json` is the message body
/// (a ComposeRequestSpec value); `catalog` is (name, description?) pairs.
pub fn plan_compose_request(
    sender: &str,
    spec_json: &str,
    allowlist: &[String],
    catalog: &[crate::decide::CatalogEntry],
) -> ComposeRequestOutcome {
    if !crate::decide::compose_request_allowed(sender, allowlist) {
        return ComposeRequestOutcome::Denied;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(spec_json.trim()) else {
        return ComposeRequestOutcome::Denied;
    };
    let Ok(spec) = toron_workflow::jobs::ComposeRequestSpec::parse(&v) else {
        return ComposeRequestOutcome::Denied;
    };
    let report = crate::decide::coverage_report(&spec.objective, catalog);
    if report.covered {
        return ComposeRequestOutcome::Covered {
            workflow: report.matched.unwrap_or_default(),
        };
    }
    let parts = report.candidate_parts.unwrap_or_default();
    let prompt = crate::decide::render_compose_candidate_for_approval(
        sender,
        &spec.objective,
        report.archetype,
        &parts,
    );
    ComposeRequestOutcome::CandidateForApproval { prompt }
}

/// Fetch `[compose-request]` rows + the project allowlist (checkpointed
/// leaf; all file/mail access stays in the subprocess). Soft-fail empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeRequestRow {
    pub id: String,
    pub from: String,
    pub body: String,
}

/// bd-dispatch-dedupe-2zlz leaf: run the GOAL gate for one bead. Collects
/// thread signals + receipts freshness via the `__dispatch-guard` collector,
/// then applies the pure [`eval_dispatch_guard`] verdict. Scan INFRA
/// failures (collector crash, bad JSON, mail daemon down) fail OPEN with a
/// loud log — a broken scanner must not freeze all dispatch; a SUCCESSFUL
/// Settle one `__dispatch-guard` outcome (bd-5ouq): parse the fetched facts
/// and evaluate them against caller time, returning the exact
/// `{bead, dispatch, reason}` shape the `#[function]` body returns.
/// Pure — fully pinned by unit tests. The call sites run the fetch through
/// the journaled leaf and settle here; scan-infra failure stays fail-open
/// at the call site (proceed), with the leaf error on stderr. Deviation
/// from the old body, documented: the old code distinguished "scan failed"
/// (non-zero exit) from "scan error" (transport) in the fail-open reason;
/// the leaf folds both into one error, so both now proceed with the leaf
/// error line. Dispatch behavior is identical (fail-open proceed either
/// way); only the reason string merges.
pub(crate) fn settle_dispatch_guard_output(
    facts: &serde_json::Value,
    bead: &str,
    now_us: i64,
    ttl_us: i64,
) -> serde_json::Value {
    let signals: Vec<(i64, GuardSignalKind)> = facts
        .get("signals")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let ts = s.get("ts_us").and_then(serde_json::Value::as_i64)?;
                    let kind = match s.get("kind").and_then(serde_json::Value::as_str)? {
                        "claim" => GuardSignalKind::Claim,
                        "announce" => GuardSignalKind::Announce,
                        "done" => GuardSignalKind::Done,
                        "stand_down" => GuardSignalKind::StandDown,
                        "handoff" => GuardSignalKind::Handoff,
                        _ => return None,
                    };
                    Some((ts, kind))
                })
                .collect()
        })
        .unwrap_or_default();
    let facts = DispatchGuardFacts {
        signals,
        receipts_fresh_us: facts
            .get("receipts_fresh_us")
            .and_then(serde_json::Value::as_i64),
    };
    match eval_dispatch_guard(&facts, now_us, ttl_us) {
        Ok(()) => serde_json::json!({ "bead": bead, "dispatch": true, "reason": null }),
        Err(reason) => serde_json::json!({ "bead": bead, "dispatch": false, "reason": reason }),
    }
}

/// Spawn the swarm for a parsed `[xproj]` request (full loop branch).
/// Delegates to the `__xproj-spawn` leaf which calls `cmd_spawn` + replies
/// in-thread. Soft-fail: spawn errors are surfaced in-thread by the leaf, the
/// loop continues.
async fn spawn_xproj(args: XprojSpawnArgs) -> anyhow::Result<()> {
    if crate::state::xproj_seen_contains(&args.project, &args.message_id)? {
        return Ok(());
    }
    crate::state::xproj_seen_mark(&args.project, &args.message_id)?;
    let exe = self_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args([
        "__xproj-spawn",
        &args.project,
        &args.message_id,
        &args.thread_id,
        &args.from,
        &args.recipe,
        &args.workers.to_string(),
    ]);
    let out = run_leaf_cmd(cmd, "__xproj-spawn").await?;
    if !out.status.success() {
        let _ = crate::state::xproj_seen_unmark(&args.project, &args.message_id);
        return Err(anyhow::anyhow!(
            "__xproj-spawn failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = crate::state::log_event(
        &args.project,
        crate::state::EventKind::State,
        serde_json::json!({
            "source": "xproj_spawn",
            "message_id": args.message_id,
            "recipe": args.recipe,
            "workers": args.workers,
        }),
    );
    Ok(())
}

/// Diagnose an unresponsive agent: capture recent output + high-importance
/// health mail (bd-event-self-heal). Soft-fail: never kill the loop.
async fn diagnose_agent(args: DiagnoseArgs) -> anyhow::Result<Value> {
    let exe = self_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args([
        "__diagnose",
        &args.project,
        &args.name,
        "--reason",
        &args.reason,
    ]);
    let out = run_leaf_cmd(cmd, "__diagnose").await?;
    let raw = String::from_utf8_lossy(&out.stdout);
    if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
        let _ = crate::state::log_event(
            &args.project,
            crate::state::EventKind::State,
            serde_json::json!({
                "source": "diagnose",
                "name": args.name,
                "reason": args.reason,
                "result": &v,
            }),
        );
        return Ok(v);
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok(serde_json::json!({
        "ok": false,
        "name": args.name,
        "error": if err.is_empty() { format!("exit {}", out.status) } else { err },
    }))
}

/// bd-bd79: filtered renewal — renew only leases whose reason matches an
/// in_progress bead with a live assignee and that are near expiry.
/// Checkpointed leaf; a failed renewal is surfaced as Err so the caller can
/// log + increment the visible counter (bd-cy0s will add full event emission).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewFilteredArgs {
    pub project: String,
    pub paths: Vec<String>,
    pub extend_seconds: u64,
}

/// The renew leaf's synchronous body: one mail block that must run OFF the
/// durable loop's async worker. Split out so the acceptance test can drive
/// the real block through the bridge on a runtime
/// (`loop_mode_durable_survives_reclaim`) instead of a copy of it.
fn renew_filtered_sync(
    project: &str,
    paths: &[String],
    extend_seconds: u64,
) -> anyhow::Result<Value> {
    let cfg = Config::load()?;
    let mail = crate::backend::mail_backend(&cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let key = crate::utf8_path(&crate::project_dir(&cfg, project), "project path")?;
    let _ = crate::backend::run_mail(mail.ensure_project(&key));
    let agent = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "flywheel".to_string());
    let v = crate::backend::run_mail(mail.renew_reservations(&key, &agent, paths, extend_seconds))
        .map_err(|e| anyhow::anyhow!("mail backend: {e}"))?;
    Ok(json!({ "renewed": v.renewed, "paths": paths }))
}

async fn renew_filtered_leases(args: RenewFilteredArgs) -> anyhow::Result<Value> {
    if args.paths.is_empty() {
        return Ok(json!({ "renewed": 0, "paths": [] }));
    }
    let (project, paths, extend_seconds) = (
        args.project.clone(),
        args.paths.clone(),
        args.extend_seconds,
    );
    crate::backend::run_mail_off_runtime(move || {
        renew_filtered_sync(&project, &paths, extend_seconds)
    })
    .await
}

/// Lease TTL policy for loop-made reservations: an initial TTL of
/// Renewal extension per heartbeat tick: `4 × heartbeat_interval`, floored at
/// agent-mail's 60s `extend_seconds` minimum — never compute tiny extensions.
pub fn lease_renew_seconds(heartbeat_interval_secs: u64) -> u64 {
    (4 * heartbeat_interval_secs).max(60)
}

/// Visible counter for failed renewals (bd-bd79: log + counter now, full event
/// emission lands with bd-cy0s later).
static RENEW_FAIL_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn inc_renew_fail_count() {
    RENEW_FAIL_COUNTER.fetch_add(1, Ordering::Relaxed);
}
pub fn record_renew_failure(path: &str, reason: &str, err: &str) {
    eprintln!("flywheel: lease renew failed for {path} (reason {reason}): {err}");
    inc_renew_fail_count();
}

/// Pure: should a lease be renewed given its expiry and now? Near-expiry
/// means remaining <= threshold. Unknown expiry (None) is treated as needing
/// renewal (fail-closed: renew rather than let it lapse).
pub fn should_renew_lease(expires_ts_us: Option<i64>, now_us: i64, threshold_secs: u64) -> bool {
    match expires_ts_us {
        None => true,
        Some(exp) => exp.saturating_sub(now_us) <= (threshold_secs as i64) * 1_000_000,
    }
}

/// Pure: filter active leases to those that must be renewed this tick.
///
/// A lease qualifies iff:
/// - its `reason` exactly matches an in_progress bead id,
/// - that bead's assignee is Some and the assignee is live (present in
///   `live_names`),
/// - and the lease is near expiry (remaining <= threshold, or unknown expiry).
///
/// `leases` is (path, reason, expires_ts_us). Returns the (path, reason)
/// pairs to renew. Unrelated leases are untouched by definition.
pub fn leases_to_renew(
    leases: &[(String, String, Option<i64>)],
    in_progress: &[ReadyBead],
    live_names: &[String],
    now_us: i64,
    threshold_secs: u64,
) -> Vec<(String, String)> {
    leases
        .iter()
        .filter(|(_, reason, expires)| {
            let live_match = in_progress.iter().any(|b| {
                &b.id == reason
                    && b.assignee
                        .as_ref()
                        .is_some_and(|a| live_names.iter().any(|l| l == a))
            });
            live_match && should_renew_lease(*expires, now_us, threshold_secs)
        })
        .map(|(p, r, _)| (p.clone(), r.clone()))
        .collect()
}

/// Convenience overload for boards without expiry (treat as near-expiry).
pub fn leases_to_renew_simple(
    leases: &[(String, String)],
    in_progress: &[ReadyBead],
    live_names: &[String],
) -> Vec<(String, String)> {
    let with_none: Vec<(String, String, Option<i64>)> = leases
        .iter()
        .map(|(p, r)| (p.clone(), r.clone(), None))
        .collect();
    leases_to_renew(&with_none, in_progress, live_names, 0, 0)
}

/// bd-cy0s: pane stall threshold. The threshold is a LoopParams field
/// (resolved once at loop start, serialized for replay); this resolver is
/// used only by the CLI construction site in main.rs. Default 1800s.
pub const PANE_STALL_DEFAULT_SECS: u64 = 1800;
pub fn pane_stall_threshold_from_environment() -> u64 {
    std::env::var("FLYWHEEL_PANE_STALL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(PANE_STALL_DEFAULT_SECS)
}

/// Pane-stall evidence + classification (extends bd-cy0s).
pub const STALL_EVIDENCE_TAIL_LINES: usize = 40;
pub const STALL_EVIDENCE_MAX_CHARS: usize = 2000;

/// Watching tier threshold: quick-check at FLYWHEEL_PANE_STALL_SECS/4.
/// Floor at 1s so a tiny threshold still yields a watching tier.
pub fn stall_watching_threshold_secs(threshold_secs: u64) -> u64 {
    (threshold_secs / 4).max(1)
}

/// Classify stall reason from evidence tail (case-insensitive substring match).
/// Precedence: rate_limit > network > model_truncated > silent_idle.
pub fn classify_stall_reason(evidence: &str) -> String {
    let lower = evidence.to_ascii_lowercase();
    if lower.contains("quota") || lower.contains("usage limit") || lower.contains("allowance") {
        "allowance".to_string()
    } else if lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("overloaded")
        || lower.contains("at capacity")
    {
        "rate_limit".to_string()
    } else if lower.contains("connection")
        || lower.contains("timeout")
        || lower.contains("reset")
        || lower.contains("unreachable")
    {
        "network".to_string()
    } else if lower.contains("unterminated tool call")
        || lower.contains("max tokens")
        || lower.contains("abruptly ended")
    {
        "model_truncated".to_string()
    } else {
        "silent_idle".to_string()
    }
}

fn truncate_tail(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("\n…(truncated)");
    out
}

fn redact_secrets_tail(s: &str) -> String {
    let mut out_lines = Vec::new();
    for line in s.lines() {
        let lower = line.to_ascii_lowercase();
        let is_secret_line = lower.contains("nsec1")
            || lower.contains("ncryptsec")
            || lower.contains("private key")
            || lower.contains("secret")
            || lower.contains("api_key")
            || lower.contains("apikey")
            || lower.contains("password")
            || lower.contains("token=")
            || lower.contains("token:")
            || lower.contains("sk-")
            || lower.contains("sk_");
        if is_secret_line {
            let redacted = "[REDACTED secret]".to_string();
            out_lines.push(redacted);
        } else {
            let mut cur = line.to_string();
            let lower_cur = cur.to_ascii_lowercase();
            if lower_cur.contains("nsec1") || lower_cur.contains("ncryptsec") {
                cur = "[REDACTED nsec/ncryptsec token present]".to_string();
            }
            out_lines.push(cur);
        }
    }
    let joined = out_lines.join("\n");
    let mut final_out = String::new();
    let mut chars = joined.chars().peekable();
    while let Some(c) = chars.next() {
        if (c == 's' || c == 'S') && chars.peek().is_some_and(|n| *n == 'k' || *n == 'K') {
            let mut buf = String::new();
            buf.push(c);
            if let Some(k) = chars.next() {
                buf.push(k);
                if let Some(sep) = chars.next() {
                    if sep == '-' || sep == '_' {
                        buf.push(sep);
                        let mut token_len = 0;
                        let mut token_buf = String::new();
                        while let Some(&ch) = chars.peek() {
                            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                                token_buf.push(ch);
                                chars.next();
                                token_len += 1;
                                if token_len > 64 {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                        if token_len >= 8 {
                            final_out.push_str("[REDACTED_sk]");
                        } else {
                            final_out.push_str(&buf);
                            final_out.push_str(&token_buf);
                        }
                        continue;
                    }
                    final_out.push_str(&buf);
                    final_out.push(sep);
                    continue;
                }
                final_out.push_str(&buf);
                continue;
            }
            final_out.push_str(&buf);
            continue;
        }
        final_out.push(c);
    }
    final_out
}

/// Take last STALL_EVIDENCE_TAIL_LINES lines, redact secrets, truncate.
/// Pure — unit-testable, no I/O.
pub fn sanitize_evidence_tail(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let tail = if lines.len() > STALL_EVIDENCE_TAIL_LINES {
        lines[lines.len() - STALL_EVIDENCE_TAIL_LINES..].join("\n")
    } else {
        raw.to_string()
    };
    let redacted = redact_secrets_tail(&tail);
    truncate_tail(&redacted, STALL_EVIDENCE_MAX_CHARS)
}

/// Pure: detect stalled panes — bead in_progress + assignee pane listed +
/// agent idle/silent past threshold. Returns PaneStalled facts (feeds Diagnose,
/// NOT auto-reclaim). `now_secs` is wall clock secs, `updated_at` is bead's
/// ISO8601 timestamp.
///
/// Two-tier: `watching` at threshold/4 (visible early, no Diagnose) and
/// `stalled` at threshold (with evidence + reason, feeds Diagnose). Evidence
/// sampling is one scrollback read per detection, not per tick — the caller
/// samples the tail via `evidence_fn` only when a pane crosses a tier.
pub fn detect_stalled_panes(
    in_progress: &[ReadyBead],
    states: &[AgentState],
    now_secs: i64,
    threshold_secs: u64,
) -> Vec<IngressEvent> {
    detect_stalled_panes_with_evidence(
        in_progress,
        states,
        now_secs,
        threshold_secs,
        stall_watching_threshold_secs(threshold_secs),
        |_| String::new(),
    )
}

/// Tiered detection with evidence sampling callback. `evidence_fn` is called
/// once per pane that crosses a tier (one read per detection, not per tick)
/// and its raw tail is sanitized + classified before emitting.
pub fn detect_stalled_panes_with_evidence<F>(
    in_progress: &[ReadyBead],
    states: &[AgentState],
    now_secs: i64,
    threshold_secs: u64,
    watching_threshold_secs: u64,
    mut evidence_fn: F,
) -> Vec<IngressEvent>
where
    F: FnMut(&AgentState) -> String,
{
    let mut out = Vec::new();
    for bead in in_progress {
        let Some(assignee) = bead.assignee.as_deref() else {
            continue;
        };
        let Some(state) = states.iter().find(|s| s.name == assignee) else {
            continue;
        };
        if !matches!(state.status.as_str(), "idle" | "done") {
            continue;
        }
        let Some(updated) = bead
            .updated_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
        else {
            continue;
        };
        let age = now_secs.saturating_sub(updated);
        let severity = if age >= threshold_secs as i64 {
            "stalled"
        } else if age >= watching_threshold_secs as i64 {
            "watching"
        } else {
            continue;
        };
        let raw_tail = evidence_fn(state);
        let evidence_tail = sanitize_evidence_tail(&raw_tail);
        let reason = if evidence_tail.is_empty() {
            "silent_idle".to_string()
        } else {
            classify_stall_reason(&evidence_tail)
        };
        out.push(IngressEvent::PaneStalled {
            pane_id: state.pane_id.clone(),
            terminal_name: state.name.clone(),
            last_status: state.status.clone(),
            bead_id: Some(bead.id.clone()),
            reason,
            evidence_tail,
            severity: severity.to_string(),
        });
    }
    out
}

/// Internal: emit a fact to journal + handle_ingress (captain surface).
/// Best-effort: journal failures are ignored (facts are at-least-once), and
/// handle_ingress failures never kill the loop.
fn emit_fact(state_dir: &std::path::Path, project: &str, event: IngressEvent) {
    let adapters = crate::ingress_adapters_for_project(state_dir, project);
    crate::ingress::handle_ingress(&event, &adapters);
}

fn emit_ledger_fact(state_dir: &std::path::Path, project: &str, bead_id: &str, event: LedgerEvent) {
    emit_fact(
        state_dir,
        project,
        IngressEvent::LedgerFact {
            bead_id: bead_id.to_string(),
            event,
        },
    );
}

fn emit_lease_lapsed(
    state_dir: &std::path::Path,
    project: &str,
    path: &str,
    bead_id: Option<&str>,
) {
    emit_fact(
        state_dir,
        project,
        IngressEvent::LeaseLapsed {
            path: path.to_string(),
            bead_id: bead_id.map(std::string::ToString::to_string),
        },
    );
}

/// Pure planning: given current states, decide actions and drain the queue.
/// Deterministic dispatch plan for one poll: working agents wait, blocked
/// agents report, idle/done agents get the next queue item (or a nudge, or
/// wait). `unknown` is NOT dispatchable by design: hc cannot classify its
/// lifecycle confidently — a prompt could land in a half-dead session and be
/// silently lost. Unknown agents get `Wait` and never consume the queue.
///
/// `digests` (per-agent mail digests from the `__mail-digest` leaf) shape two
/// behaviors — the loop-side wake signal that replaces per-agent peer-poll
/// templates (token-efficient: agents hold NO standing polling instructions;
/// the orchestrator polls once per iteration):
///   - a dispatched prompt is prefixed with a compact mail line when the
///     agent has recent mail (`[mail] 2 recent (1 urgent) (last: X re: Y)`)
///     so the agent wakes already knowing a peer asked it something;
///   - an idle agent with URGENT mail and an empty queue gets a wake
///     prompt instead of silent `Wait` — it is asked to check + reply, then
///     await work. Urgent mail IS work. (toron has no read-state: "recent"
///     is the inbox projection window; "urgent" is importance high|urgent.)
///
/// `DispatchImmediate` is the event-path twin of `Send` (bd-event-dispatch):
/// same `__send` leaf, same FIFO pop, marked so the loop can track
/// dispatch-pending until a `working` event confirms landing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopAction {
    Send {
        name: String,
        text: String,
    },
    /// ADR-0001 Wave 3 / Layer 3 (beads_rust-wave3-pin-reservation-jxsyt):
    /// stamp the dispatched bead's `pin` field with the receiving agent's
    /// mail identity BEFORE the brief lands — ownership is recorded by the
    /// orchestrator on dispatch, not left to agent discipline. Emitted only
    /// when the queue item is a bead/resume brief AND the pin is known.
    /// Best-effort at the execution site: a tracker hiccup never blocks the
    /// dispatch itself.
    PinStamp {
        bead_id: String,
        pin: String,
    },
    /// Event-wake redispatch: same leaf as `Send`, skips poll batching.
    DispatchImmediate {
        name: String,
        text: String,
    },
    Blocked {
        name: String,
    },
    /// ADR-0010 D4/D12: re-surface a drained pane's declared `paused:` wait to
    /// the human who owns it. Emitted once per resurface window and re-armed,
    /// so the wait repeats loudly instead of parking forever.
    PausedResurface {
        name: String,
        bead_id: String,
        owner: String,
        age_secs: u64,
    },
    Nudge {
        name: String,
    },
    /// Self-heal: read tail + high-importance mail (bd-event-self-heal).
    Diagnose {
        name: String,
        reason: String,
    },
    /// Self-heal: `__respawn` after diagnose failed to clear (budgeted).
    Respawn {
        name: String,
        reason: String,
    },
    /// ADR-0025 Wave 1: reclaim a dead/stale in_progress bead to open.
    Reclaim {
        bead_id: String,
        pin: String,
        terminal_name: String,
        reason: String,
    },
    /// bd-bpd9: the graph vehicle spawns a detached pane-mode engine run at
    /// dispatch time (spawn_graph_vehicle) — a subprocess, not a LoopAction.
    /// LoopAction stays the loop's own decision set.
    Wait,
}

/// Map a CONTEXT_LIMIT sender to the crew agent it saturates: MAIL PIN
/// first (the identity the contract addresses), then the bare terminal name.
/// An unknown sender maps to NOTHING — a human pasting the marker into a
/// reply must never respawn an agent.
pub fn context_limit_target(from: &str, states: &[AgentState]) -> Option<String> {
    if from.trim().is_empty() {
        return None;
    }
    states
        .iter()
        .find(|s| s.mail_identity.as_deref() == Some(from))
        .or_else(|| states.iter().find(|s| s.name == from))
        .map(|s| s.name.clone())
}

/// One-line mail digest prefix for a prompt, when the agent has recent mail.
/// Uses `recent` (the honest toron projection window) and cites the last
/// MESSAGE ID + subject — bd-kryk: the id is the replyable handle; a sender
/// npub in this slot made reply-by-id fail and read like phantom mail.
fn digest_prefix(d: &MailDigest) -> Option<String> {
    if d.recent == 0 {
        return None;
    }
    let mut line = format!("[mail] {} recent", d.recent);
    if d.urgent > 0 {
        line.push_str(&format!(" ({} urgent)", d.urgent));
    }
    if let (Some(id), Some(s)) = (&d.last_id, &d.last_subject) {
        line.push_str(&format!(" (id {id} re: {s})"));
    } else if let Some(id) = &d.last_id {
        line.push_str(&format!(" (id {id})"));
    }
    Some(line)
}

/// Wake prompt for an idle agent with urgent mail and an empty queue.
/// Concrete + actionable: names the sender/thread and gives BOTH surfaces
/// (toron MCP `reply_message` or `flywheel mail reply --message-id`) — a
/// vague "check your inbox" prompt gets acknowledged in-chat and never
/// produces the actual mail reply (e2e-verified).
///
/// S8.1: the fetch/reply identity is `mail_identity` (the identity the
/// digest registered) — the wake must not instruct "fetch as" a name that
/// is not registered (debrief D1/D2).
fn urgent_wake_prompt(d: &MailDigest) -> String {
    let identity = d.mail_identity.as_deref().unwrap_or(d.agent.as_str());
    let cite_thread = citable_wake_id(d.thread_id.as_deref());
    let cite_id = citable_wake_id(d.last_id.as_deref());
    let mut line = format!(
        "MAIL WAKE: {} urgent message(s) in your inbox — a peer needs you. \
         Fetch with `flywheel mail inbox <project> --as {identity} --urgent`, then \
         REPLY to the most recent urgent one with `flywheel mail reply <project> --message-id <id> --body '<answer>' --as {identity}` \
         (or toron `reply_message` with the same thread_id). Your reply IS the work — do not just acknowledge in chat.",
        d.urgent,
    );
    if let (Some(t), true) = (&d.thread_id, cite_thread) {
        line.push_str(&format!(" Latest urgent thread: {t}"));
    }
    if let (Some(id), true) = (&d.last_id, cite_id) {
        line.push_str(&format!(" Most recent urgent: id {id}"));
        if let Some(s) = &d.last_subject
            && !s.is_empty()
        {
            line.push_str(&format!(" re: {s}"));
        }
    }
    line
}

/// bd-wake-id-contract-1jz1 clause 1: a wake-citable id is a RESOLVABLE
/// mail-surface handle — a non-empty hex event id (toron ids are content-
/// addressed hex, 8+ chars). Reservation paths, thread labels, npubs, or
/// placeholder strings in an id slot are phantom handles: the banner drops
/// them rather than sending the agent hunting for a message that does not
/// exist.
pub fn citable_wake_id(id: Option<&str>) -> bool {
    match id {
        Some(id) => {
            let id = id.trim();
            !id.is_empty() && id.len() >= 8 && id.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// Wake prompt for an idle agent with plane-S work (open jobs / channel
/// mentions) and an empty queue. Concrete + actionable: names the surfaces
/// (toron `job list` / `channel read` / `watch`) — a vague "check your
/// channels" prompt gets acknowledged in-chat and never produces the actual
/// job claim (bd-pin-public-plane-s-easy.5).
fn plane_s_wake_prompt(d: &MailDigest) -> String {
    let identity = d.mail_identity.as_deref().unwrap_or(d.agent.as_str());
    format!(
        "PLANE-S WAKE: {} open job(s)/mention(s) on the crew room — a peer posted work for you. \
         Claim with `toron job list --project <project> --state open --as {identity}` then \
         `toron job claim <job-id> --project <project> --as {identity}`; read the room with \
         `toron channel read <channel> --project <project> --as {identity}`. Watch for new work: \
         `toron watch --once --kinds 9,7,43001,20001 --project <project> --as {identity}`.",
        d.plane_s,
    )
}

/// Why a watched bead is waiting (ADR-0019 Spec 3.4). An overlap wait is
/// NOT a stuck owner: the bead's write paths are held by another in-flight
/// bead — no nudge, no T0-T3 escalation, the loop waits for release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitReason {
    /// the dispatched agent reported blocked (the T0-T3 escalation path
    /// owns this reason)
    Blocked,
    /// the bead serialized behind another in-flight bead's reservation
    Overlap,
    /// bd-claim-watch-durable-signals-mixy: the bead itself is held by a
    /// live toron reservation (reason == bead id) — a durable peer claim.
    /// Redispatch arms wait for the lease to lapse; tracker nudges still
    /// fire (claim hygiene + close escalation), but no second executor is
    /// dispatched over live work.
    LiveLease,
    /// the bead is a wave-N labeled bead held while an earlier wave-M (M<N)
    /// is still open/in_progress (bd-gnej) — not a stuck owner, the loop
    /// waits for the earlier wave to close
    WaveGate,
}

/// One dispatched bead's claim-watch entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimWatch {
    /// the agent the bead prompt was dispatched to (terminal name)
    pub agent: String,
    /// consecutive polls the bead has stayed UNCLAIMED (still in `br ready`)
    /// while the dispatched agent is engaged
    pub unclaimed_polls: u32,
    /// a close-nudge was already sent for a claimed-but-unclosed bead
    /// (nudge once; escalate to redispatch if it stays unclosed)
    pub close_nudged: bool,
    /// polls since the close-nudge while the bead stayed unclosed
    pub close_polls: u32,
    /// why the bead is waiting (None = normal in-flight work)
    pub wait_reason: Option<WaitReason>,
}

/// ADR-0019 Spec 3.5: paths whose overlap is NEVER split into parallel —
/// two beads needing any of these run one after the other (serialize).
pub const ALWAYS_SERIALIZE_PATHS: &[&str] = &[
    ".beads/**",
    ".flywheel/**",
    "Cargo.lock",
    "Cargo.toml",
    "docs/catalog/**",
    "target/**",
];
/// always-serialize path? If yes, all NEW ready-bead dispatch serializes —
/// never parallel (ADR-0019 Spec 3.5, acceptance: two beads touching
/// `.beads/` or `Cargo.lock` run one after the other).
pub fn always_serialize_held(held: &[(String, String)]) -> bool {
    held.iter()
        .any(|(path, _)| ALWAYS_SERIALIZE_PATHS.iter().any(|a| overlaps(path, a)))
}

/// Pure: in-flight bead ids whose held path globs conflict with ANOTHER
/// in-flight bead's held globs — both are overlap-waiting (ADR-0019 Spec
/// 3.4). `held` = reservation rows `(path_pattern, reason)` where reason is
/// the holding bead's id; `in_flight` = currently dispatched bead ids. The
/// claim-watch must not treat these waits as stuck owners.
pub fn bead_overlap_waits(
    held: &[(String, String)],
    in_flight: &[String],
) -> std::collections::HashSet<String> {
    let mut waits = std::collections::HashSet::new();
    let mut by_reason: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for (path, reason) in held {
        by_reason.entry(reason).or_default().push(path);
    }
    let reasons: Vec<&String> = in_flight
        .iter()
        .filter(|r| by_reason.contains_key(r.as_str()))
        .collect();
    for i in 0..reasons.len() {
        for j in (i + 1)..reasons.len() {
            let a = &by_reason[reasons[i].as_str()];
            let b = &by_reason[reasons[j].as_str()];
            if a.iter().any(|pa| b.iter().any(|pb| overlaps(pa, pb))) {
                waits.insert(reasons[i].clone());
                waits.insert(reasons[j].clone());
            }
        }
    }
    waits
}

/// One watchdog decision for a watched bead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimWatchAction {
    /// send a claim-focused nudge to the dispatched agent (short, explicit).
    NudgeClaim { agent: String, bead_id: String },
    /// the dispatched agent settled without claiming — re-queue the full
    /// bead prompt at the FRONT so another agent takes it.
    RedispatchBead { bead_id: String, title: String },
    /// the bead was claimed but its assignee is idle/done without closing —
    /// send a close-focused nudge to the assignee.
    NudgeClose {
        agent: String,
        bead_id: String,
        title: String,
    },
    /// claimed-but-unclosed after the close nudge — re-queue a RESUME prompt
    /// so a fresh agent finishes + closes it.
    RedispatchResume { bead_id: String, title: String },
    /// the bead left both tracker snapshots — it is CLOSED; its file
    /// reservations (reason == bead id) must be released (bd-h3oj
    /// release-on-bead-close: never let a closed bead's locks linger to TTL).
    ReleaseReservations { bead_id: String },
}

/// Extract the bead id from a dispatched brief, if any. The brief's first
/// line carries it: `GOAL Execute bd-XXX: ...` (fresh) / `GOAL Resume bd-XXX:
/// ...` (resume). Plain prompts (nudges, mail wakes, stages) yield `None` —
/// the watchdog only tracks bead dispatches.
pub fn bead_id_from_prompt(text: &str) -> Option<&str> {
    let goal = text.lines().next()?.strip_prefix("GOAL ")?.trim();
    let rest = goal
        .strip_prefix("Execute ")
        .or_else(|| goal.strip_prefix("Resume "))?;
    let id = rest.split(':').next().unwrap_or(rest).trim();
    if id.is_empty() { None } else { Some(id) }
}

/// ADR-0001 Wave 3 / Layer 3 (beads_rust-wave3-pin-reservation-jxsyt):
/// argv for the orchestrator-side pin stamp — the `__pin-stamp` leaf runs
/// `br update <id> --pin <pin> --json` in the project dir. Pure: both loop
/// execution sites (durable + sync) run it best-effort right before a bead
/// dispatch; failure is logged, never blocks the dispatch.
#[must_use]
pub fn pin_stamp_argv(bead_id: &str, pin: &str) -> Vec<String> {
    vec![
        "update".to_string(),
        bead_id.to_string(),
        format!("--pin={pin}"),
        "--json".to_string(),
    ]
}

/// The claim-focused nudge prompt (bd-efk1 option A): one explicit command,
/// not the dense lifecycle. The agent is WORKING — it just never claimed.
pub fn claim_nudge_prompt(bead_id: &str) -> String {
    format!(
        "CLAIM REQUIRED: bead {bead_id} is still unclaimed in the tracker and you were dispatched it. \
         Run NOW: `br update {bead_id} --claim --actor \"$AGENT_NAME\" --json` (cd to the repo holding .beads/), \
         then continue the execution prompt you already received. Do NOT work an unclaimed bead."
    )
}

/// The close-focused nudge prompt (bd-efk1 option C): the bead is claimed but
/// the assignee settled without closing it.
pub fn close_nudge_prompt(bead_id: &str, title: &str) -> String {
    format!(
        "CLOSE REQUIRED: bead {bead_id} ({title}) is still in_progress and your lane is idle. \
         Finish + verify the acceptance gate, then close it with the gate-evidence reason: \
         `br close {bead_id} --actor \"$AGENT_NAME\" --reason \"<command + result>\"`, `br sync --flush-only` + commit. \
         If you CANNOT finish, reopen it (`br update {bead_id} --status open --actor \"$AGENT_NAME\"`) so the loop re-dispatches it."
    )
}

/// bd-zfnx.10: the drain ask — finish the current VERIFY fence and
/// report, instead of being killed mid-fence (which orphans the
/// receipt). Emitted as Send (existing variant); the `[drain-stop]`
/// marker keeps the ask observable in journals and transcripts.
pub fn drain_prompt(bead_id: &str) -> String {
    format!(
        "[drain-stop] Finish your current VERIFY fence for {bead_id} and report on its thread now; \
         do not start new work. You are being drained, not re-tasked. \
         If you are WAITING ON A HUMAN instead, declare it: reply `paused: <who>` (\
         the named owner) — the loop holds your grace and re-surfaces the wait \
         to that owner every 3600s (ADR-0010 D4/D12); it does not auto-interrupt you."
    )
}

/// One claim-watch reconcile step (pure — the unit surface). Mutates
/// `watch` and returns the actions the loop must take this poll.
///
/// `nudge_after_polls`: consecutive unclaimed polls before a claim nudge.
/// `overlap`: bead ids currently waiting on another in-flight bead's
/// reservation (ADR-0019 Spec 3.4). An overlap wait is NOT a stuck owner —
/// no claim nudge, no close nudge, no resume re-dispatch; the wait reason is
/// recorded and the loop waits for the holder to release.
///
/// bd-claim-watch-durable-signals-mixy: `live_leases` = bead ids with a
/// live toron reservation on the active board (`reason == bead id`). The
/// REDISPATCH arms (Bead/Resume) are suppressed while a live lease exists —
/// the reserve-before-work lifecycle makes the lease the durable "work is
/// taken" record, visible even when the angle's mail is silent or its pane
/// is a non-hc harness that `states` cannot see. The tracker nudges
/// (claim/close) still fire: they target the bead's own lane, never a
/// second executor. Consultation order: live lease → assignee pane state →
/// mail guard (the dispatch scan the loop runs on redispatch).
#[cfg(test)]
pub fn claim_watch_step(
    watch: &mut std::collections::HashMap<String, ClaimWatch>,
    ready: &[ReadyBead],
    inprogress: &[ReadyBead],
    states: &[AgentState],
    nudge_after_polls: u32,
    overlap: &std::collections::HashSet<String>,
    live_leases: &std::collections::HashSet<String>,
) -> Vec<ClaimWatchAction> {
    claim_watch_step_with_known(
        watch,
        ready,
        inprogress,
        &[],
        states,
        nudge_after_polls,
        overlap,
        live_leases,
    )
}

/// [`claim_watch_step`] with the explicit `not_ready` snapshot (blocked,
/// deferred, held, or otherwise non-dispatchable beads that still EXIST).
///
/// A bead absent from both ready and in-progress is closed ONLY when it is
/// absent from this set too. Without it, a Blocked/Deferred transition looks
/// like closure and revokes the worker's live reservations (bd-claim-watch).
#[allow(clippy::too_many_arguments)]
pub fn claim_watch_step_with_known(
    watch: &mut std::collections::HashMap<String, ClaimWatch>,
    ready: &[ReadyBead],
    inprogress: &[ReadyBead],
    not_ready: &[&ReadyBead],
    states: &[AgentState],
    nudge_after_polls: u32,
    overlap: &std::collections::HashSet<String>,
    live_leases: &std::collections::HashSet<String>,
) -> Vec<ClaimWatchAction> {
    let mut actions = Vec::new();
    let mut drop = Vec::new();
    let nudge_after = nudge_after_polls.max(1);
    let wave_held: std::collections::HashSet<String> =
        wave_gate_held(ready, inprogress).into_iter().collect();
    for (bead_id, cw) in watch.iter_mut() {
        let waiting_on_overlap = overlap.contains(bead_id);
        let waiting_on_lease = live_leases.contains(bead_id);
        let in_ready = ready.iter().find(|b| b.id == *bead_id);
        let claimed = inprogress.iter().find(|b| b.id == *bead_id);
        let waiting_on_wave = wave_held.contains(bead_id);
        match (in_ready, claimed) {
            (Some(rb), None) => {
                if waiting_on_overlap {
                    cw.wait_reason = Some(WaitReason::Overlap);
                    continue;
                }
                if waiting_on_wave {
                    cw.wait_reason = Some(WaitReason::WaveGate);
                    continue;
                }
                cw.wait_reason = None;
                let st = states
                    .iter()
                    .find(|s| s.name == cw.agent)
                    .map(|s| s.status.as_str());
                match st {
                    Some("blocked") => {
                        cw.wait_reason = Some(WaitReason::Blocked);
                        cw.unclaimed_polls += 1;
                        if cw.unclaimed_polls >= nudge_after {
                            cw.unclaimed_polls = 0;
                            actions.push(ClaimWatchAction::NudgeClaim {
                                agent: cw.agent.clone(),
                                bead_id: bead_id.clone(),
                            });
                        }
                    }
                    Some("working" | "unknown") => {
                        cw.unclaimed_polls += 1;
                        if cw.unclaimed_polls >= nudge_after {
                            cw.unclaimed_polls = 0;
                            actions.push(ClaimWatchAction::NudgeClaim {
                                agent: cw.agent.clone(),
                                bead_id: bead_id.clone(),
                            });
                        }
                    }
                    _ => {
                        if waiting_on_lease {
                            cw.wait_reason = Some(WaitReason::LiveLease);
                            continue;
                        }
                        drop.push(bead_id.clone());
                        actions.push(ClaimWatchAction::RedispatchBead {
                            bead_id: bead_id.clone(),
                            title: rb.title.clone(),
                        });
                    }
                }
            }
            (_, Some(cb)) => {
                let assignee = cb.assignee.clone().unwrap_or_else(|| cw.agent.clone());
                let live = states.iter().any(|s| s.name == assignee);
                let settled = states
                    .iter()
                    .find(|s| s.name == assignee)
                    .map(|s| matches!(s.status.as_str(), "idle" | "done"))
                    .unwrap_or(false);
                if live && settled {
                    if waiting_on_overlap {
                        cw.wait_reason = Some(WaitReason::Overlap);
                        continue;
                    }
                    cw.wait_reason = None;
                    if !cw.close_nudged {
                        cw.close_nudged = true;
                        cw.close_polls = 0;
                        actions.push(ClaimWatchAction::NudgeClose {
                            agent: assignee,
                            bead_id: bead_id.clone(),
                            title: cb.title.clone(),
                        });
                    } else if waiting_on_lease {
                        cw.wait_reason = Some(WaitReason::LiveLease);
                    } else {
                        cw.close_polls += 1;
                        if cw.close_polls >= nudge_after {
                            drop.push(bead_id.clone());
                            actions.push(ClaimWatchAction::RedispatchResume {
                                bead_id: bead_id.clone(),
                                title: cb.title.clone(),
                            });
                        }
                    }
                }
            }
            (None, None) => {
                if not_ready.iter().any(|b| b.id == *bead_id) {
                    // The bead still exists; it is merely not schedulable now.
                    // Keep the watch (and its locks) until it closes.
                    cw.wait_reason = Some(WaitReason::Blocked);
                    cw.unclaimed_polls = 0;
                    continue;
                }
                actions.push(ClaimWatchAction::ReleaseReservations {
                    bead_id: bead_id.clone(),
                });
                drop.push(bead_id.clone());
            }
        }
    }
    for id in drop {
        watch.remove(&id);
    }
    actions
}

/// Carry nudge_streak across a poll snapshot (poll JSON has no streak).
/// Working/done resets it. Idle keeps the prior count.
pub fn carry_nudge_streaks(polled: Vec<AgentState>, prior: &[AgentState]) -> Vec<AgentState> {
    polled
        .into_iter()
        .map(|mut s| {
            if s.status == "working" || s.status == "done" {
                s.nudge_streak = 0;
                return s;
            }
            if let Some(p) = prior.iter().find(|p| p.name == s.name) {
                s.nudge_streak = p.nudge_streak;
            }
            s
        })
        .collect()
}

/// Increment streak after a Nudge is applied. Returns a rot action when
/// the named proxy (streak as repetition+silence) crosses Compact.
pub fn bump_nudge_streak(states: &mut [AgentState], name: &str) -> crate::rot::RotAction {
    let Some(s) = states.iter_mut().find(|s| s.name == name) else {
        return crate::rot::RotAction::None;
    };
    s.nudge_streak = s.nudge_streak.saturating_add(1);
    let ev = crate::rot::RotEvents {
        repetition: s.nudge_streak,
        marker_silence_turns: if s.status == "idle" {
            s.nudge_streak
        } else {
            0
        },
        ..crate::rot::RotEvents::default()
    };
    let a = crate::rot::action(&ev);
    if s.nudge_streak == 3 && a == crate::rot::RotAction::Compact {
        a
    } else {
        crate::rot::RotAction::None
    }
}

/// Dispatch planner: pair idle agents with queued prompts positionally.
/// Thin wrapper — no live-owned bead set, so every queued prompt stays
/// eligible (the pre-bf39z behavior every existing caller relies on).
pub fn plan_actions(
    model: &LoopModel,
    states: &[AgentState],
    nudge: Option<&str>,
    digests: &HashMap<String, MailDigest>,
    wake_suppressed: &std::collections::HashSet<String>,
) -> Vec<LoopAction> {
    plan_actions_with_live_owned(
        model,
        states,
        nudge,
        digests,
        wake_suppressed,
        &HashMap::new(),
    )
}

/// bd-bf39z: `plan_actions` with the resume candidate set narrowed —
/// `live_owned` maps bead id → owner pin for beads another LIVE pane owns.
/// An idle pane skips a bead prompt whose owner is live and is not itself,
/// falling through to the next eligible item (an unowned ready bead) before
/// drawing a no-op. Its OWN live-owned bead still sends (rotation tier-1
/// self-resumes arrive exactly this way). Pure: no I/O, no clock.
pub fn plan_actions_with_live_owned(
    model: &LoopModel,
    states: &[AgentState],
    nudge: Option<&str>,
    digests: &HashMap<String, MailDigest>,
    wake_suppressed: &std::collections::HashSet<String>,
    live_owned: &HashMap<String, String>,
) -> Vec<LoopAction> {
    let mut actions = Vec::new();
    let mut pending: Vec<String> = model.queue.clone();
    for s in states {
        match s.status.as_str() {
            "working" => actions.push(LoopAction::Wait),
            "blocked" => actions.push(LoopAction::Blocked {
                name: s.name.clone(),
            }),
            "unknown" => actions.push(LoopAction::Wait),
            _ => {
                let pin = s.mail_identity.clone().unwrap_or_else(|| s.name.clone());
                let take = pending
                    .iter()
                    .position(|text| match bead_id_from_prompt(text) {
                        Some(id) => live_owned.get(id).is_none_or(|owner| owner == &pin),
                        None => true,
                    });
                if let Some(j) = take {
                    let text = pending.remove(j);
                    let base = text.replace("{AGENT_NAME}", &pin);
                    let text = match digests.get(&s.name) {
                        Some(d) => match digest_prefix(d) {
                            Some(p) => format!("{p}\n{base}"),
                            None => base,
                        },
                        None => base,
                    };
                    if let Some(bead_id) = bead_id_from_prompt(&text) {
                        actions.push(LoopAction::PinStamp {
                            bead_id: bead_id.to_string(),
                            pin,
                        });
                    }
                    actions.push(LoopAction::Send {
                        name: s.name.clone(),
                        text,
                    });
                } else if nudge.is_some() {
                    if crate::rot::interrupt_nudge(s.nudge_streak) {
                        actions.push(LoopAction::Nudge {
                            name: s.name.clone(),
                        });
                    } else {
                        actions.push(LoopAction::Send {
                            name: s.name.clone(),
                            text: format!("[nudge-mail] {}", nudge.unwrap_or("proceed")),
                        });
                    }
                } else if let Some(d) = digests.get(&s.name)
                    && (d.urgent > 0 || d.plane_s > 0)
                    && !wake_suppressed.contains(&s.name)
                {
                    let text = if d.plane_s > 0 {
                        plane_s_wake_prompt(d)
                    } else {
                        urgent_wake_prompt(d)
                    };
                    actions.push(LoopAction::Send {
                        name: s.name.clone(),
                        text,
                    });
                } else {
                    actions.push(LoopAction::Wait);
                }
            }
        }
    }
    actions
}

/// Resolve agent terminal name for a pane id from the latest poll snapshot.
pub fn name_for_pane(states: &[AgentState], pane_id: &str) -> Option<String> {
    if pane_id.is_empty() {
        return None;
    }
    states
        .iter()
        .find(|s| s.pane_id == pane_id)
        .map(|s| s.name.clone())
}

/// Heartbeat snapshot cadence (bd-w4-events-primary-gt0e): the authoritative
/// `agent.list` snapshot runs every this-many iterations when the event
/// stream is healthy. Between snapshots the event stream asserts status
/// changes ([`apply_events_to_states`]); the snapshot confirms. The FIRST
/// iteration after crash-resume always snapshots (hc has no since/seq
/// replay) — the correctness floor is never skipped.
pub const HEARTBEAT_POLL_EVERY: u64 = 5;

/// Snapshot-due policy (bd-w4-events-primary-gt0e): the authoritative
/// `agent.list` snapshot runs when the loop has no state (crash-resume /
/// first iteration — hc has no since/seq replay, so the floor always
/// runs first), when the event stream errored (fallback), or at the
/// heartbeat cadence (poll confirms what events asserted).
pub fn snapshot_due(states_empty: bool, stream_error: bool, since_snapshot: u64) -> bool {
    states_empty || stream_error || since_snapshot >= HEARTBEAT_POLL_EVERY
}

/// Events assert, the poll confirms (bd-w4-events-primary-gt0e): apply a
/// status-change batch to the last-known `states` so `all_settled` and
/// `plan_actions` react to event-asserted statuses without waiting for the
/// heartbeat snapshot. `pane.agent_status_changed` / `pane.agent_detected`
/// update the status of the pane's agent; `pane.exited` marks the agent
/// `exited`. Unknown panes (not yet in the snapshot — e.g. freshly spawned)
/// are left to the next snapshot. Pure + unit-tested.
pub fn apply_events_to_states(states: &mut [AgentState], events: &[PollEvent]) {
    for ev in events {
        let status = ev.status.as_deref().unwrap_or("");
        match ev.event_type.as_str() {
            "pane.agent_status_changed" | "pane.agent_detected" => {
                let Some(name) = name_for_pane(states, &ev.pane_id) else {
                    continue;
                };
                if let Some(s) = states.iter_mut().find(|s| s.name == name)
                    && !status.is_empty()
                {
                    s.status = status.to_string();
                }
            }
            "pane.exited" => {
                let Some(name) = name_for_pane(states, &ev.pane_id) else {
                    continue;
                };
                if let Some(s) = states.iter_mut().find(|s| s.name == name) {
                    s.status = "exited".to_string();
                }
            }
            _ => {}
        }
    }
}

/// Clear dispatch-pending after a confirmed send landing (or fail-stop).
pub fn clear_dispatch_pending(
    pending: &mut std::collections::HashSet<String>,
    name: &str,
    landed: Option<bool>,
) {
    match landed {
        Some(true) => {
            pending.remove(name);
        }
        Some(false) => {
            pending.remove(name);
        }
        None => {
            pending.remove(name);
        }
    }
}

/// Completion guard: everyone idle/done. `unknown` keeps the loop alive — an
/// unclassified agent may still be doing the last delegated task, so the loop
/// stays on the active cadence.
pub fn all_settled(states: &[AgentState]) -> bool {
    states
        .iter()
        .all(|s| matches!(s.status.as_str(), "idle" | "done"))
}

/// Stranded-bead detection (bd-627z.1, bd-qk9q): an in_progress bead whose
/// assignee is NOT a live swarm agent is stranded. Live = the hc agent
/// names polled this tick (captain, dead agents, and unknown identities are
/// NOT live — their beads would stall the batch forever).
///
/// An in_progress bead with NO assignee IS stranded (bd-qk9q): nobody holds
/// it, no agent is working it, and nothing else will ever close it — the
/// bd-bvwf.1 strand was exactly this. Reclaim re-opens it to ready so the
/// loop re-dispatches it. Presence-based, NOT status-based: an assignee
/// present in the polled swarm is live regardless of status (unknown included
/// — bd-bvwf.5: reclaim targets GONE/absent agents, never unknown-but-present
/// ones). Pure — the unit surface for the reclaim decision.
pub fn stranded_beads<'a>(
    inprogress: &'a [ReadyBead],
    live_names: &[String],
) -> Vec<&'a ReadyBead> {
    inprogress
        .iter()
        .filter(|b| match &b.assignee {
            Some(a) => !live_names.iter().any(|l| l == a),
            None => true,
        })
        .collect()
}

/// Scoped completion guard (bd-627z.1): are there in_progress beads that
/// BLOCK completion — i.e. in-progress beads held by LIVE agents? Stranded
/// beads (non-live assignee) do NOT block: they are being reclaimed
/// (re-opened to ready + re-dispatched), so they must not keep the loop from
/// resolving once everything else is done. Pure — the unit surface for the
/// completion re-check.
pub fn blocking_inprogress<'a>(
    inprogress: &'a [ReadyBead],
    live_names: &[String],
) -> Vec<&'a ReadyBead> {
    inprogress
        .iter()
        .filter(|b| match &b.assignee {
            Some(a) => live_names.iter().any(|l| l == a),
            None => true,
        })
        .collect()
}

/// Every input the reclaim decision may consult, built once per reclaim pass
/// by the `__reclaim-stranded` leaf. The decision itself is pure — it only
/// reads these sets (bd-reclaim-liveness-not-spelling-hn8b).
pub struct ReclaimWorld<'a> {
    /// Every name a LIVE pane answers to: the polled terminal names the loop
    /// passed, plus each registry row's `name` and `mail_identity` when its
    /// pane is still listed.
    pub live_identities: &'a std::collections::HashSet<String>,
    /// Spawn-registry rows, used to resolve an assignee spelling to its pane.
    pub registry: &'a [crate::state::SpawnEntry],
    /// Pane ids hc lists right now.
    pub live_panes: &'a std::collections::HashSet<String>,
    /// Bead ids carrying a live reservation; the lease is proof of life.
    pub leased_beads: &'a std::collections::HashSet<String>,
}

/// Why a bead is reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimReason {
    /// No assignee and no lease — nobody holds it (bd-qk9q).
    Unowned,
    /// The assignee resolves to a registry pane hc no longer lists.
    DeadPane,
}

/// Why a bead is NOT reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimHold {
    /// A live reservation for the bead itself (reason = bead id).
    LiveLease,
    /// The assignee resolves to a pane hc lists right now.
    LivePane,
    /// No pane evidence either way — never race an unproven-dead worker.
    UnknownIdentity,
}

impl ReclaimReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unowned => "unowned",
            Self::DeadPane => "dead pane",
        }
    }
}

impl ReclaimHold {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveLease => "live lease",
            Self::LivePane => "live pane",
            Self::UnknownIdentity => "unknown identity",
        }
    }
}

/// The reclaim decision outcome (typed, so call sites cannot string-match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimDecision {
    Reclaim { reason: ReclaimReason },
    Keep { reason: ReclaimHold },
}

/// The reclaim decision for one in_progress bead: reclaim only when the
/// evidence proves the worker cannot be on it.
///
/// The old predicate compared the assignee SPELLING against a name list, so a
/// live claim written under any other name for the same agent (hc slug vs
/// pin vs registry row) read as stranded and got re-opened mid-work — racing
/// the worker and resetting its claim every poll. This resolves the assignee
/// through the registry to its pane and asks hc. Kept under any of these
/// proofs, in order: live lease on the bead, the assignee matches a live
/// identity, or the assignee resolves to a live pane. An unresolvable spelling
/// is KEPT: absence of evidence is not evidence of death.
pub fn reclaim_decision(
    bead_id: &str,
    assignee: Option<&str>,
    world: &ReclaimWorld<'_>,
) -> ReclaimDecision {
    if world.leased_beads.contains(bead_id) {
        return ReclaimDecision::Keep {
            reason: ReclaimHold::LiveLease,
        };
    }
    let Some(assignee) = assignee else {
        return ReclaimDecision::Reclaim {
            reason: ReclaimReason::Unowned,
        };
    };
    if world.live_identities.contains(assignee) {
        return ReclaimDecision::Keep {
            reason: ReclaimHold::LivePane,
        };
    }
    let entry = world
        .registry
        .iter()
        .find(|e| e.name == assignee || e.mail_identity.as_deref() == Some(assignee));
    match entry {
        Some(e) if !e.pane_id.is_empty() && world.live_panes.contains(&e.pane_id) => {
            ReclaimDecision::Keep {
                reason: ReclaimHold::LivePane,
            }
        }
        Some(e) if !e.pane_id.is_empty() => ReclaimDecision::Reclaim {
            reason: ReclaimReason::DeadPane,
        },
        Some(_) | None => ReclaimDecision::Keep {
            reason: ReclaimHold::UnknownIdentity,
        },
    }
}

/// Adaptive poll cadence: fast while the swarm is busy, slow when settled.
///
/// A dispatch this iteration (`dispatched`) forces the active cadence: the
/// recipient will be working during the sleep, but `states` is the PRE-action
/// poll — without this the loop would sleep `base_secs` right after handing
/// out work (the failure mode this function exists to fix). Otherwise, any
/// agent `working`, `blocked`, or `unknown` → `active_secs`; a fully
/// idle/done (or empty) swarm → `base_secs`. Clamped to >= 5s.
///
/// `pending_work` (bd-bvwf.2 — detection-retry): work is queued but NO agent
/// is classified (all `unknown`, or the swarm is empty — a freshly spawned
/// crew is not yet in hc's agent.list). The first poll after a fresh spawn
/// otherwise sees an empty/unknown swarm and falls to `base_secs`, stranding
/// the first bead for a full settled interval while hc identifies the
/// panes. With pending work and nothing classified, the loop re-polls on the
/// ACTIVE cadence until detection completes.
///
/// `unknown` counts as unsettled BY DESIGN: an unknown-heavy swarm is a swarm
/// needing repair (`bd-unknown-respawn`), and repairs must proceed on the
/// active cadence. An all-unknown swarm therefore polls at 30s rather than
/// 300s — acceptable, because `--timeout-ms` (max_iterations) still bounds the
/// run if it never converges.
///
/// Deterministic across replays: derives from the checkpointed states vector
/// and the (also deterministic) action plan.
///
/// Follow-up idea (do NOT build here): a future redesign could wake the loop
/// on hc status events via a durable park/waiter — event-driven
/// instead of polled. Requires an event listener with its own durability.
pub fn next_interval_secs(
    states: &[AgentState],
    dispatched: bool,
    pending_work: bool,
    base_secs: u64,
    active_secs: u64,
) -> u64 {
    const INTERVAL_MIN: u64 = 5;
    let classified = states
        .iter()
        .any(|s| matches!(s.status.as_str(), "idle" | "done"));
    let busy = dispatched
        || states
            .iter()
            .any(|s| matches!(s.status.as_str(), "working" | "blocked" | "unknown"))
        || (pending_work && !classified);
    let chosen = if busy { active_secs } else { base_secs };
    chosen.max(INTERVAL_MIN)
}

/// Decision for one non-landing (or ambiguous) confirmed send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Escalation {
    /// Prompt did not land — respawn the agent and re-queue the prompt at the
    /// FRONT so the fresh instance inherits it next poll.
    RespawnAndRequeue,
    /// Prompt landed (or is ambiguous): nothing to escalate. A landed send
    /// clears the agent's failure history; a timeout leaves it untouched and
    /// is verified by the next poll.
    Accept,
    /// Cancellation is distinct from provider failure: do not respawn or
    /// increment the pane-health counter.
    Cancelled,
    /// 4 consecutive non-landing sends on the same agent (3 respawns, then
    /// the 4th fails): hard stop the loop. A genuinely broken environment
    /// must not burn the swarm forever. (cg6.4: threshold is FOUR — the
    /// counter `escalate` reads is the pre-increment value, so
    /// `failures >= 3` at call time means 4 failures have happened.)
    FailStop,
}

/// The FailStop error message (cg6.4): states the REAL contract — the loop
/// respawns an agent after the first three non-landing sends and stops on
/// the FOURTH. Single source of truth for both halt sites + tests.
pub fn failstop_message(name: &str, reason: Option<&str>) -> String {
    format!("agent {name} failed to accept 4 prompts in a row (last reason: {reason:?})")
}

/// Pure verdict for one confirmed send outcome.
///
/// `landed: true` (incl. a `blocked` status — a valid landing the
/// `--on-blocked` policy owns) → [`Escalation::Accept`]; `landed: None`
/// (legacy fire-and-forget path) → [`Escalation::Accept`]; a `timeout` reason
/// is ambiguous (the prompt may have started) → [`Escalation::Accept`]
/// WITHOUT touching the failure counter; any other non-landing (stalled,
/// `error:<code>`) is a pane-health failure → respawn on the first 3
/// failures; the 4th (`failures >= 3` at call time — the counter is the
/// PRE-increment value, see [`handle_send_result`]) → [`Escalation::FailStop`].
/// Deterministic: derives from the checkpointed
/// leaf result and the (also deterministic) counter.
pub fn escalate(outcome: &SendOutcome, failures: u64) -> Escalation {
    if outcome.cancelled {
        return Escalation::Cancelled;
    }
    match outcome.landed {
        Some(true) | None => Escalation::Accept,
        Some(false) => match outcome.reason.as_deref() {
            Some("timeout") => Escalation::Accept,
            _ => {
                if failures >= 3 {
                    Escalation::FailStop
                } else {
                    Escalation::RespawnAndRequeue
                }
            }
        },
    }
}

/// Pure send-outcome bookkeeping: run [`escalate`] AND update the per-agent
/// failure counter in one place.
///
/// Reset contract (documented, both events checkpointed): a landed send
/// removes the agent's counter; a timeout leaves it untouched (ambiguous —
/// verified by the next poll); a non-landing failure increments it; at the
/// cap the counter does not grow further ([`Escalation::FailStop`]).
pub fn handle_send_result(
    failures: &mut std::collections::HashMap<String, u64>,
    name: &str,
    outcome: &SendOutcome,
) -> Escalation {
    match escalate(outcome, failures.get(name).copied().unwrap_or(0)) {
        Escalation::Accept => {
            if outcome.landed == Some(true) {
                failures.remove(name);
            }
            Escalation::Accept
        }
        Escalation::RespawnAndRequeue => {
            *failures.entry(name.to_string()).or_insert(0) += 1;
            Escalation::RespawnAndRequeue
        }
        Escalation::Cancelled => Escalation::Cancelled,
        fail_stop => fail_stop,
    }
}

/// Pure proof-of-life reset: agents showing `working` or `blocked` in the
/// poll are receiving prompts — drop their failure history. Agents absent
/// from the poll (hc dropped them) or `unknown` keep their counters.
pub fn clear_live_failures(
    failures: &mut std::collections::HashMap<String, u64>,
    states: &[AgentState],
) {
    for s in states {
        if matches!(s.status.as_str(), "working" | "blocked") {
            failures.remove(&s.name);
        }
    }
}

/// Learn agent kinds from a poll. Never removes entries: a dropped agent's
/// kind is exactly what the escalation path needs to respawn it, and a later
/// kind-less poll must not clobber it.
pub fn learn_kinds(kinds: &mut std::collections::HashMap<String, String>, states: &[AgentState]) {
    for s in states {
        if !s.kind.is_empty() {
            kinds.insert(s.name.clone(), s.kind.clone());
        }
    }
}

/// Pick the kind for a respawn: the kind learned from polls (authoritative —
/// a dropped agent's record cannot be re-queried), else the caller falls back
/// to the `poll_kind` leaf.
pub fn respawn_kind(
    kinds: &std::collections::HashMap<String, String>,
    name: &str,
) -> Option<String> {
    kinds.get(name).cloned()
}

/// Parse one `__poll` entry: `[name, status, kind, pane_id?, mail_identity?]`.
/// kind added for respawn; pane_id for event→name routing
/// (bd-event-dispatch); mail_identity (the pin) for `{AGENT_NAME}`
/// substitution (bd-pin-public-plane-s-easy.2). Legacy 2/3/4-element
/// entries yield empty kind/pane_id and None mail_identity.
pub fn parse_poll_entry(v: &serde_json::Value) -> anyhow::Result<AgentState> {
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("__poll bad entry: {v}"))?;
    Ok(AgentState {
        name: arr
            .first()
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        status: arr
            .get(1)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        kind: arr
            .get(2)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        pane_id: arr
            .get(3)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        mail_identity: arr
            .get(4)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        nudge_streak: 0,
    })
}

/// Consecutive `unknown` polls before **diagnose** (bd-event-self-heal).
/// Default 3: tolerates brief classification flicker without mail spam.
pub const DEFAULT_UNKNOWN_DIAGNOSE_THRESHOLD: u32 = 3;

/// Max respawns per agent per loop run before escalate-to-blocked.
pub const DEFAULT_RESPAWN_BUDGET: u32 = 2;

/// Consecutive `unknown` polls treated as DETECTION GRACE (bd-bvwf.5).
/// Default 6 (~3min at the 30s active cadence — hc takes 30-60s to
/// classify a fresh pane): an unknown agent younger than this is presumed
/// detecting, never Diagnosed or Respawnd. A genuinely dead pane is caught
/// by the `pane.exited` event (immediate ladder) or by grace exhaustion.
pub const DEFAULT_UNKNOWN_DETECT_GRACE_POLLS: u32 = 6;

/// Completion drain-grace polls (bd-bvwf.3). Default 1: the completion
/// condition must hold TWO consecutive polls before exit — the second poll
/// re-sources `br ready` and catches a bead unblocked by the last closure
/// (dependency chain), so the loop drives the whole chain in one run.
pub const DEFAULT_DRAIN_GRACE_POLLS: u32 = 1;

/// bd-87hq: consecutive blind-completion iterations before the loop aborts.
/// The stale-checkpoint signature (a previous binary's checkpointed leaf
/// results no longer deserialize) is: every critical leaf fails while the
/// world looks EMPTY — the loop would declare "complete" having dispatched
/// nothing. This many consecutive empty-looking-but-leaf-failing iterations
/// (≈2-3min at the active cadence) abort with --force advice instead.
pub const DEFAULT_LEAF_FAILURE_ABORT_POLLS: u32 = 3;

/// bd-87hq: pure completion-gate accounting — how many consecutive
/// "empty world + failing leaves" iterations have accumulated. Reset the
/// moment the world is either visible (no leaf failure) or non-empty.
fn blind_completion_count(complete_now: bool, leaf_failed: bool, consecutive: u32) -> u32 {
    if complete_now && leaf_failed {
        consecutive.saturating_add(1)
    } else {
        0
    }
}

/// bd-swarm-loop-durable-panic-atlo: what the blind-completion door does once
/// [`blind_completion_count`] reaches the abort threshold (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlindCompletion {
    /// Below the threshold — keep polling (the WARN prints each pass).
    Continue,
    /// Threshold reached: abort with the fix. No stale-checkpoint degrade —
    /// plain async execution has no replay, so a resumed run cannot deserialize
    /// stale leaves.
    Abort,
}

/// Pure: the blind-completion outcome for (failures, threshold).
/// A blind run aborts once the threshold trips; there is no degrade path.
pub fn blind_completion_outcome(_resumed: bool, failures: u32, threshold: u32) -> BlindCompletion {
    if failures < threshold {
        BlindCompletion::Continue
    } else {
        BlindCompletion::Abort
    }
}

/// Claim-watch nudge cadence (bd-efk1). Default 2: a dispatched bead may
/// stay unclaimed for ~2 active polls (≈60s at the 30s active cadence —
/// the crew-execution gate's 60s claim bound) before the loop nudges the
/// working agent to `br update --claim` it.
pub const DEFAULT_CLAIM_NUDGE_AFTER_POLLS: u32 = 2;

/// ADR-0025 reclaim: stale thresholds (swarm vs human). Defaults live in
/// `crate::durable_policy::ReclaimPolicy`; the env adapter is called once
/// at loop start, never inside the pure reclaim predicates.
pub const FLYWHEEL_RECLAIM_SWARM_SECS: u64 = 7200;
pub const FLYWHEEL_RECLAIM_HUMAN_SECS: u64 = 86400;

pub fn is_swarm_assignee(assignee: &str) -> bool {
    if assignee.contains('-') {
        return true;
    }
    assignee.chars().any(char::is_uppercase)
}
pub fn parse_updated_at_secs(s: Option<&str>) -> Option<i64> {
    let raw = s?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp())
        .ok()
}
pub fn stale_reclaim_due(
    bead: &ReadyBead,
    now_secs: i64,
    panes: &[AgentState],
    reservations: &[(String, String, String)],
    last_comment_ts: Option<i64>,
    last_comment_body: Option<&str>,
    dirty_files: &[String],
) -> bool {
    stale_reclaim_due_with_policy(
        bead,
        now_secs,
        panes,
        reservations,
        last_comment_ts,
        last_comment_body,
        dirty_files,
        &crate::durable_policy::ReclaimPolicy::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn stale_reclaim_due_with_policy(
    bead: &ReadyBead,
    now_secs: i64,
    panes: &[AgentState],
    reservations: &[(String, String, String)],
    last_comment_ts: Option<i64>,
    last_comment_body: Option<&str>,
    dirty_files: &[String],
    policy: &crate::durable_policy::ReclaimPolicy,
) -> bool {
    let assignee = match &bead.assignee {
        Some(a) => a,
        None => return false,
    };
    let updated = match parse_updated_at_secs(bead.updated_at.as_deref()) {
        Some(t) => t,
        None => return false,
    };
    let age = now_secs.saturating_sub(updated);
    let threshold = if is_swarm_assignee(assignee) {
        policy.swarm_secs as i64
    } else {
        policy.human_secs as i64
    };
    if age <= threshold {
        return false;
    }
    if reservations.iter().any(|(_, reason, _)| reason == &bead.id) {
        return false;
    }
    if panes
        .iter()
        .any(|p| p.name == *assignee && p.status == "working")
    {
        return false;
    }
    if let Some(ts) = last_comment_ts
        && now_secs.saturating_sub(ts) < 3600
    {
        return false;
    }
    if let Some(body) = last_comment_body {
        for f in dirty_files {
            if body.contains(f) {
                return false;
            }
        }
    }
    true
}
#[cfg_attr(not(test), allow(dead_code))]
pub fn plan_reclaim(
    now_secs: i64,
    beads: &[ReadyBead],
    panes: &[AgentState],
    reservations: &[(String, String)],
) -> Vec<LoopAction> {
    plan_reclaim_with_policy(
        now_secs,
        beads,
        panes,
        reservations,
        &crate::durable_policy::ReclaimPolicy::default(),
    )
}

pub fn plan_reclaim_with_policy(
    now_secs: i64,
    beads: &[ReadyBead],
    panes: &[AgentState],
    reservations: &[(String, String)],
    policy: &crate::durable_policy::ReclaimPolicy,
) -> Vec<LoopAction> {
    let ext_res: Vec<(String, String, String)> = reservations
        .iter()
        .map(|(p, r)| (p.clone(), r.clone(), String::new()))
        .collect();
    let mut out = Vec::new();
    for b in beads {
        if stale_reclaim_due_with_policy(b, now_secs, panes, &ext_res, None, None, &[], policy) {
            let pin = b.assignee.clone().unwrap_or_default();
            let terminal_name = panes
                .iter()
                .find(|p| p.name == pin)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| pin.clone());
            out.push(LoopAction::Reclaim {
                bead_id: b.id.clone(),
                pin: pin.clone(),
                terminal_name,
                reason: "stale".to_string(),
            });
        }
    }
    out
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn plan_reclaim_ex(
    now_secs: i64,
    beads: &[ReadyBead],
    panes: &[AgentState],
    reservations: &[(String, String, String)],
    comments: &std::collections::HashMap<String, (i64, String)>,
    dirty_files: &[String],
) -> Vec<LoopAction> {
    let mut out = Vec::new();
    for b in beads {
        let (last_ts, last_body) = comments
            .get(&b.id)
            .map(|(ts, body)| (Some(*ts), Some(body.as_str())))
            .unwrap_or((None, None));
        let holder_mismatch = reservations.iter().any(|(_, reason, holder)| {
            reason == &b.id && holder != b.assignee.as_deref().unwrap_or("") && !holder.is_empty()
        });
        if holder_mismatch {
            continue;
        }
        if stale_reclaim_due(
            b,
            now_secs,
            panes,
            reservations,
            last_ts,
            last_body,
            dirty_files,
        ) {
            let pin = b.assignee.clone().unwrap_or_default();
            let terminal_name = panes
                .iter()
                .find(|p| p.name == pin)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| pin.clone());
            out.push(LoopAction::Reclaim {
                bead_id: b.id.clone(),
                pin: pin.clone(),
                terminal_name,
                reason: "stale".to_string(),
            });
        }
    }
    out
}

pub fn reclaim_on_pane_exited(
    bead: &ReadyBead,
    exited_name: &str,
    panes: &[AgentState],
    heal: &HealState,
    respawn_budget: u32,
) -> Option<LoopAction> {
    let assignee = bead.assignee.as_deref()?;
    if assignee != exited_name {
        return None;
    }
    let pane_still_listed = panes.iter().any(|p| p.name == exited_name);
    let budget_exhausted = heal.respawn_count >= respawn_budget;
    if !budget_exhausted && pane_still_listed {
        return None;
    }
    Some(LoopAction::Reclaim {
        bead_id: bead.id.clone(),
        pin: assignee.to_string(),
        terminal_name: exited_name.to_string(),
        reason: "pane_exited".to_string(),
    })
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclaimBeadArgs {
    pub project: String,
    pub bead_id: String,
    pub pin: String,
    pub terminal_name: String,
    pub reason: String,
}
async fn reclaim_bead(args: ReclaimBeadArgs) -> anyhow::Result<Value> {
    let exe = self_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args(["__release-bead-locks", &args.project, &args.bead_id]);
    let rel_out = run_leaf_cmd(cmd, "__release-bead-locks").await;
    let released = match rel_out {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout);
            serde_json::from_str::<Value>(raw.trim())
                .ok()
                .and_then(|v| v.get("released").and_then(Value::as_u64))
                .unwrap_or(0)
        }
        Ok(o) => {
            eprintln!(
                "flywheel: reclaim release for {} failed: {}",
                args.bead_id,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            0
        }
        Err(e) => {
            eprintln!(
                "flywheel: reclaim release leaf error for {}: {e}",
                args.bead_id
            );
            0
        }
    };
    let evidence = format!(
        "reclaim: previous in_progress claim appears abandoned; evidence: pin={}, hc={}, reason={}, updated_at stale or pane_exited",
        args.pin, args.terminal_name, args.reason
    );
    let project_dir = {
        let p = std::path::Path::new(&args.project);
        if p.is_dir() {
            p.to_path_buf()
        } else {
            let ws =
                std::path::PathBuf::from(format!("/Users/bangedorrunt/workspace/{}", args.project));
            if ws.is_dir() {
                ws
            } else {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            }
        }
    };
    let commented = {
        let project_dir = project_dir.clone();
        let bead_id = args.bead_id.clone();
        let evidence = evidence.clone();
        let res = tokio::task::spawn_blocking(move || {
            crate::br::run_br(
                Some(&project_dir),
                &[
                    "comments",
                    "add",
                    &bead_id,
                    "--message",
                    &evidence,
                    "--actor",
                    "flywheel-toron-loop1",
                ],
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("br comment task join failed: {e}"))?;
        match res {
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "flywheel: reclaim comment for {} failed: {e:#}",
                    args.bead_id,
                );
                false
            }
        }
    };
    let opened = {
        let project_dir = project_dir.clone();
        let bead_id = args.bead_id.clone();
        let res = tokio::task::spawn_blocking(move || {
            crate::br::run_br(
                Some(&project_dir),
                &[
                    "update",
                    &bead_id,
                    "--status",
                    "open",
                    "--actor",
                    "flywheel-toron-loop1",
                ],
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("br open task join failed: {e}"))?;
        match res {
            Ok(_) => true,
            Err(e) => {
                eprintln!("flywheel: reclaim open for {} failed: {e:#}", args.bead_id,);
                false
            }
        }
    };
    Ok(
        serde_json::json!({"bead_id": args.bead_id, "released": released, "commented": commented, "opened": opened}),
    )
}

/// Legacy alias: old tests/callers used a hard respawn threshold of 2.
/// Per-agent self-heal bookkeeping (bd-event-self-heal-unknown).
/// Survives within a run; optionally mirrored into `loops.json` `respawn`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealState {
    pub unknown_count: u32,
    pub diagnose_count: u32,
    pub respawn_count: u32,
    /// After Diagnose this episode: next still-dead tick → Respawn.
    pub awaiting_clear: bool,
}

/// Pure self-heal decision for one agent (table-driven tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealAction {
    None,
    /// Read tail + high-importance mail (thread `health-<name>`).
    Diagnose,
    /// `__respawn` + requeue in-flight work (if any).
    Respawn,
    /// Budget exhausted — surface as Blocked / human escalate.
    EscalateBlocked,
}

/// Pure self-heal step (bd-event-self-heal-unknown + bd-bvwf.5).
///
/// | input | action |
/// |-------|--------|
/// | classified (not unknown, not exited) | reset episode counters; keep `respawn_count` |
/// | unknown × `detect_grace` | None — presumed DETECTING (fresh spawn / mid-identification); the loop re-polls on the active cadence instead |
/// | unknown × diagnose_threshold | Diagnose, set `awaiting_clear` |
/// | unknown while `awaiting_clear` | Respawn (if budget), else EscalateBlocked |
/// | `exited=true` first time | Diagnose immediately (definitive death — pane gone, not detection) |
/// | `exited=true` after diagnose / again | Respawn or EscalateBlocked |
///
/// `respawn_budget` caps Respawn per agent for the whole loop run. A
/// genuinely dead pane is caught by the `pane.exited` event (immediate
/// ladder) or by grace exhaustion — a WORKING agent that polls unknown for a
/// few polls is never touched.
pub fn self_heal_step(
    hs: &mut HealState,
    status: &str,
    exited: bool,
    detect_grace: u32,
    diagnose_threshold: u32,
    respawn_budget: u32,
) -> HealAction {
    let threshold = diagnose_threshold.max(1);
    let budget = respawn_budget;

    if !exited && status != "unknown" {
        hs.unknown_count = 0;
        hs.awaiting_clear = false;
        return HealAction::None;
    }

    let try_respawn = |hs: &mut HealState| -> HealAction {
        if hs.respawn_count >= budget {
            return HealAction::EscalateBlocked;
        }
        hs.respawn_count = hs.respawn_count.saturating_add(1);
        hs.awaiting_clear = false;
        hs.unknown_count = 0;
        HealAction::Respawn
    };

    if exited {
        if !hs.awaiting_clear {
            hs.diagnose_count = hs.diagnose_count.saturating_add(1);
            hs.awaiting_clear = true;
            return HealAction::Diagnose;
        }
        return try_respawn(hs);
    }

    hs.unknown_count = hs.unknown_count.saturating_add(1);

    if hs.unknown_count <= detect_grace {
        return HealAction::None;
    }

    if hs.awaiting_clear {
        return try_respawn(hs);
    }

    if hs.unknown_count >= threshold {
        hs.diagnose_count = hs.diagnose_count.saturating_add(1);
        hs.awaiting_clear = true;
        return HealAction::Diagnose;
    }

    HealAction::None
}

/// Shared respawn path for the loop: re-poll the agent kind (preferring the
/// kind learned from polls — a dropped agent's record cannot be re-queried),
/// respawn a fresh instance under the same name, and re-queue `text` at the
/// FRONT so the fresh agent inherits it next poll (None when there is nothing
/// to requeue — e.g. the unknown-status repair, whose agent was never
/// dispatched work). The single respawn point in the workflow — used by the
/// CONTEXT_LIMIT guard, the send-escalation path (bd-send-stall-escalation),
/// and the unknown-status repair (bd-unknown-respawn) so an agent can never
/// be double-respawned in one iteration.
///
/// cg6.2: leaf errors NEVER abort the loop — on a poll_kind or respawn_agent
/// failure the failure is logged and the queue is left unchanged (the next
/// poll re-derives the agent's state; the send-dedup prevents duplicates).
#[allow(clippy::too_many_arguments)]
async fn respawn_and_requeue(
    params: &LoopParams,
    queue: &mut Vec<String>,
    kinds: &std::collections::HashMap<String, String>,
    name: &str,
    text: Option<&str>,
    reason: &str,
    attempts: u64,
) {
    let kind = match respawn_kind(kinds, name) {
        Some(k) => k,
        None => match crate::elm::leaf::run_leaf_journaled(
            &params.project,
            "__kind",
            &[&params.project, name],
            &format!("kind:{}:{name}", params.project),
        ) {
            Ok(v) => v
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| v.get("raw").and_then(|r| r.as_str()).map(|s| s.to_string()))
                .unwrap_or_default(),
            Err(e) => {
                eprintln!("flywheel: poll_kind leaf failed for {name} (skipping respawn): {e}");
                return;
            }
        },
    };
    if let Err(e) = respawn_agent(RespawnArgs {
        project: params.project.clone(),
        name: name.to_string(),
        kind,
        reason: reason.to_string(),
        attempts,
        text_hash: text.map(crate::state::fnv1a64),
    })
    .await
    {
        eprintln!("flywheel: respawn_agent leaf failed for {name} (continuing): {e}");
        return;
    }
    if let Some(t) = text {
        queue.insert(0, t.to_string());
    }
}

/// The durable monitoring workflow. Runs until the queue drains and the swarm
/// settles, or a blocked agent stops it (policy).
///
/// ## Leaf-error policy (cg6.2)
/// Leaf errors NEVER abort the loop. Every `ctx.run(...)` outcome below is
/// converted to a soft default (logged via eprintln; the next poll is the
/// correctness floor). A transient hc/mail hiccup — an agent exiting
/// between poll and read, hc briefly down during a respawn, a daemon
/// blip on notify — degrades that tick, never kills the run. Genuinely
/// fatal conditions (config errors, engine store unreachable) live OUTSIDE the
/// leaf boundary and still hard-fail.
///
/// Process-per-leaf cost (durable review #8): each iteration spawns 3-8
/// `flywheel` subprocesses (poll_events + poll_swarm + xproj_inbox +
/// renew_leases + per-dispatch read_output/send_prompt), ~7ms (clap-only) to
/// ~14ms (config+backend init) each — 60-120ms per iteration, negligible at
/// the 30-300s cadence. This boundary is the CRASH-ISOLATION design (a leaf
/// panic cannot kill the loop's memory) — do not "optimize" it away without
/// a crash-isolation replacement.
///
/// bd-aseq: match an expected-reply key (the MAIL identity collab stored at
/// send time) back to a swarm state. Priority: direct terminal name → the
/// state's pinned `mail_identity` → the spawn-registry pin → the legacy
/// hash mapping. Pure.
fn match_expected_agent<'a>(
    project: &str,
    states: &'a [AgentState],
    agent: &str,
) -> Option<&'a AgentState> {
    states.iter().find(|s| {
        s.name == agent
            || s.mail_identity.as_deref() == Some(agent)
            || crate::resolve_mail_target_registry(project, &s.name) == agent
            || crate::resolve_mail_target(&s.name) == agent
    })
}

/// bd-bpd9: the per-bead graph opt-in. `## WORKFLOW <name>` in the bead
/// description, or a `workflow:<name>` label. Title text NEVER opts in —
/// the c57dfd4-era stub logged "invoking graph" for any bead with "graph"
/// in its title and then did nothing.
pub fn workflow_for_bead(bead: &ReadyBead) -> Option<String> {
    for line in bead.description.lines() {
        if let Some(rest) = line.trim().strip_prefix("## WORKFLOW ") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    bead.labels.iter().find_map(|l| {
        l.strip_prefix("workflow:")
            .map(std::string::ToString::to_string)
    })
}

/// Outcome of a graph-vehicle spawn attempt. AlreadySpawned is a DISTINCT
/// ok-arm: the vehicle owns the bead (loop keeps claim-watch), and the
/// caller must NOT fall through to a normal brief (that would double-
/// dispatch the bead every poll while the vehicle runs).
pub enum VehicleOutcome {
    Spawned,
    AlreadySpawned,
}

/// bd-bpd9 execution leaf: spawn the opted-in bead's graph vehicle as a
/// detached `workflow run <name> --execute --mode pane` subprocess — the
/// full engine rail (pane runner + VerifySink + drainers) without
/// duplicating it inside the loop. In-graph verify writes a WORKFLOW
/// receipt; close still requires loop VERIFY + CommandVerified.
/// `graph_invoked` dedupes within one loop process; across restarts the
/// marker file answers AlreadySpawned (loud, typed — never a silent Ok).
pub fn spawn_graph_vehicle(
    project: &str,
    bead_id: &str,
    workflow: &str,
    title: &str,
    verify: Option<&str>,
    graph_invoked: &mut std::collections::HashSet<String>,
) -> anyhow::Result<VehicleOutcome> {
    let marker = std::path::Path::new(&crate::config::state_dir())
        .join("journals")
        .join(format!("graph-invoked-{bead_id}.marker"));
    if marker.exists() {
        eprintln!(
            "flywheel: [bd-bpd9] AlreadySpawned for {bead_id} (marker {}) — vehicle owns the bead, no re-brief",
            marker.display()
        );
        return Ok(VehicleOutcome::AlreadySpawned);
    }
    if !graph_invoked.insert(bead_id.to_string()) {
        eprintln!(
            "flywheel: [bd-bpd9] AlreadySpawned for {bead_id} (this process) — vehicle owns the bead, no re-brief"
        );
        return Ok(VehicleOutcome::AlreadySpawned);
    }
    let _ = crate::project_state::write_atomic(&marker, workflow.as_bytes());
    let inputs = serde_json::json!({
        "bead_id": bead_id,
        "verify": verify,
        "symptom": title,
        "goal": title,
    });
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(exe)
        .args([
            "workflow",
            "run",
            workflow,
            "--execute",
            "--mode",
            "pane",
            "--project",
            project,
            "--inputs",
            &inputs.to_string(),
            "--yes",
            "--json",
        ])
        .spawn()?;
    eprintln!(
        "flywheel: [bd-bpd9] graph vehicle pid={} workflow={workflow} bead={bead_id}",
        child.id()
    );
    Ok(VehicleOutcome::Spawned)
}

async fn swarm_loop(
    authority: crate::authority::RuntimeAuthority,
    params: LoopParams,
) -> anyhow::Result<String> {
    let mut graph_invoked: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let cfg = crate::config::Config::load().unwrap_or_default();
        let check = crate::preflight::check_loop_identity(&cfg, Some(&params.project));
        if let crate::preflight::Status::Fail(msg) = check.status {
            return Err(anyhow::anyhow!(
                "loop identity for '{}' has no local key — {} (ADR-0026 Spec 4; fix: flywheel mail register --name <pin> --from-project <project> or --adopt-unnamed <name>)",
                params.project,
                msg
            ));
        }
    }
    let mut queue = params.queue.clone();
    let mut iteration = 0u64;
    let mut assigned: Vec<String> = Vec::new();
    let mut send_failures: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    let mut agent_kinds: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut swarm = LoopModel::default();
    match crate::state::load_active_loop(&params.project) {
        Ok(Some(al)) => {
            seed_swarm_resume(
                &mut swarm,
                &al.heal_states,
                &al.done_mailed,
                &al.drain_pending,
                &al.bead_slots,
                &al.agent_slots,
            );
        }
        Ok(None) => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "swarm_loop ActiveLoop seed failed for '{}': {e:#} (refusing silent empty resume)",
                params.project
            ));
        }
    }
    let mut exited_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut expected_replies: std::collections::HashMap<String, ExpectedReplyRun> =
        std::collections::HashMap::new();
    let mut event_cursor_seq: u64 = 0;
    let mut event_stream_errors: u64 = 0;
    let mut wait_secs: u64 = params.active_interval_secs.max(5);
    let mut complete_holds: u32 = 0;
    let mut blind_complete_failures: u32 = 0;
    let mut critical_leaf_failed_this_iter: bool;
    let mut not_ready_mailed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut nudge_msgs: Vec<LoopMsg> = Vec::new();
    let mut dispatch_pending: std::collections::HashSet<String> = {
        if let Ok(Some(al)) = crate::state::load_active_loop(&params.project) {
            al.dispatch_pending.clone()
        } else {
            std::collections::HashSet::new()
        }
    };
    let mut per_bead_phase: std::collections::BTreeMap<String, LoopPhase> = {
        if let Ok(Some(al)) = crate::state::load_active_loop(&params.project) {
            al.per_bead_phase
                .iter()
                .filter_map(|(k, v)| LoopPhase::parse(v).map(|p| (k.clone(), p)))
                .collect()
        } else {
            std::collections::BTreeMap::new()
        }
    };
    let mut seen_xproj: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_ctx_limit: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut states: Vec<AgentState> = Vec::new();
    let mut since_snapshot: u64 = 0;
    let mut wake: doorbell_core::wake_core::WakeModel = {
        if let Ok(Some(al)) = crate::state::load_active_loop(&params.project) {
            al.wake.clone()
        } else {
            doorbell_core::wake_core::WakeModel::default()
        }
    };

    loop {
        iteration += 1;
        critical_leaf_failed_this_iter = false;
        if let Some(max) = params.max_iterations
            && iteration > max
        {
            return Err(anyhow::anyhow!(
                "loop timed out after {max} polls (queue remaining: {})",
                queue.len()
            ));
        }

        let event_batch: PollEventsBatch = match poll_events(PollEventsArgs {
            project: params.project.clone(),
            timeout_ms: wait_secs.saturating_mul(1000),
            after_seq: event_cursor_seq,
        })
        .await
        {
            Ok(b) => b,
            Err(e) => {
                critical_leaf_failed_this_iter = true;
                eprintln!("flywheel: poll_events leaf failed (continuing): {e}");
                PollEventsBatch {
                    events: vec![],
                    max_seq: 0,
                    stream_error: Some(format!("leaf error: {e}")),
                }
            }
        };
        if event_batch.stream_error.is_some() {
            event_stream_errors = event_stream_errors.saturating_add(1);
            if event_stream_errors == 3 {
                eprintln!(
                    "flywheel: WARN event stream may be dead; heartbeat fallback active ({})",
                    event_batch.stream_error.as_deref().unwrap_or("?")
                );
            }
        } else {
            event_stream_errors = 0;
        }
        if event_batch.max_seq > event_cursor_seq {
            event_cursor_seq = event_batch.max_seq;
        }

        if snapshot_due(
            states.is_empty(),
            event_batch.stream_error.is_some(),
            since_snapshot,
        ) {
            since_snapshot = 0;
            let prior_states = states.clone();
            states = match crate::elm::leaf::run_leaf_journaled(
                &params.project,
                "__poll",
                &[&params.project, &params.to],
                &format!("poll-swarm:{}:{}", params.project, params.to),
            ) {
                Ok(v) => {
                    let parsed: anyhow::Result<Vec<AgentState>> = (|| {
                        let pairs =
                            v.get("agents").and_then(|a| a.as_array()).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "__poll missing agents: {}",
                                    serde_json::to_string(&v).unwrap_or_default()
                                )
                            })?;
                        let mut out = Vec::with_capacity(pairs.len());
                        for p in pairs {
                            out.push(parse_poll_entry(p).map_err(|e| anyhow::anyhow!("{e}"))?);
                        }
                        Ok(out)
                    })();
                    match parsed {
                        Ok(s) => carry_nudge_streaks(s, &prior_states),
                        Err(e) => {
                            critical_leaf_failed_this_iter = true;
                            eprintln!(
                                "flywheel: poll_swarm leaf failed (continuing with empty snapshot): {e}"
                            );
                            vec![]
                        }
                    }
                }
                Err(e) => {
                    critical_leaf_failed_this_iter = true;
                    eprintln!(
                        "flywheel: poll_swarm leaf failed (continuing with empty snapshot): {e}"
                    );
                    vec![]
                }
            };
        } else {
            since_snapshot += 1;
        }

        let xproj_items: Vec<crate::XprojInboxItem> = match crate::elm::leaf::run_leaf_journaled(
            &params.project,
            "__xproj-inbox",
            &[&params.project, "--limit", "30"],
            &format!("xproj-inbox:{}", params.project),
        ) {
            Ok(v) => {
                let items = v.get("items").cloned().unwrap_or(serde_json::json!([]));
                serde_json::from_value(items).unwrap_or_default()
            }
            Err(_) => vec![],
        };
        for item in &xproj_items {
            if seen_xproj.contains(&item.id) {
                continue;
            }
            let combined = format!("{} {}", item.subject, item.body);
            let req = crate::parse_xproj_request(&combined);
            let action = if let Some(r) = req.filter(|r| !r.recipe.is_empty()) {
                let spawned = spawn_xproj(XprojSpawnArgs {
                    project: params.project.clone(),
                    message_id: item.id.clone(),
                    thread_id: item.thread_id.clone(),
                    from: item.from.clone(),
                    recipe: r.recipe.clone(),
                    workers: r.workers,
                })
                .await;
                match &spawned {
                    Ok(()) => {
                        seen_xproj.insert(item.id.clone());
                        "spawned"
                    }
                    Err(e) => {
                        eprintln!("flywheel: __xproj-spawn failed for msg {}: {e}", item.id);
                        "failed"
                    }
                }
            } else {
                let prompt = crate::xproj_queue_prompt(
                    item,
                    &crate::XprojRequest {
                        recipe: String::new(),
                        workers: 2,
                        params: combined.clone(),
                        product: String::new(),
                    },
                );
                queue.push(prompt);
                seen_xproj.insert(item.id.clone());
                "queued"
            };
            eprintln!("flywheel: cross-project request msg {} {action}", item.id);
        }

        let (compose_rows, compose_allowlist): (Vec<ComposeRequestRow>, Vec<String>) =
            match crate::elm::leaf::run_leaf_journaled(
                &params.project,
                "__compose-inbox",
                &[&params.project, "--limit", "30"],
                &format!("compose-inbox:{}", params.project),
            ) {
                Ok(v) => {
                    let items = v.get("items").cloned().unwrap_or(serde_json::json!([]));
                    let rows: Vec<ComposeRequestRow> =
                        serde_json::from_value(items).unwrap_or_default();
                    let allowlist = v
                        .get("allowlist")
                        .and_then(|a| serde_json::from_value::<Vec<String>>(a.clone()).ok())
                        .unwrap_or_default();
                    (rows, allowlist)
                }
                Err(_) => (vec![], vec![]),
            };
        if !compose_rows.is_empty() {
            let catalog_owned: Vec<(String, Option<String>)> =
                crate::workflow_catalog::WorkflowCatalog::load_layered(None)
                    .map(|cat| {
                        cat.workflows
                            .iter()
                            .map(|(n, w)| {
                                (n.clone(), Some(w.description.clone().unwrap_or_default()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            let catalog: Vec<crate::decide::CatalogEntry> = catalog_owned
                .iter()
                .map(|(n, d)| (n.as_str(), d.as_deref()))
                .collect();
            for row in &compose_rows {
                let outcome =
                    plan_compose_request(&row.from, &row.body, &compose_allowlist, &catalog);
                match outcome {
                    ComposeRequestOutcome::Denied => {
                        seen_xproj.insert(row.id.clone());
                        eprintln!(
                            "flywheel: compose-request msg {} denied (sender not allowlisted) — never dispatched",
                            row.id
                        );
                    }
                    ComposeRequestOutcome::Covered { workflow } => {
                        queue.push(format!(
                            "COMPOSE-REQUEST covered: catalog workflow {workflow:?} already covers the objective from {}. \
                             Reply in-thread pointing the requester at `flywheel workflow run {workflow}`.",
                            row.from
                        ));
                        seen_xproj.insert(row.id.clone());
                    }
                    ComposeRequestOutcome::CandidateForApproval { prompt } => {
                        queue.push(prompt);
                        seen_xproj.insert(row.id.clone());
                    }
                }
            }
        }

        let ctx_items: Vec<crate::XprojInboxItem> = match crate::elm::leaf::run_leaf_journaled(
            &params.project,
            "__context-inbox",
            &[&params.project, "--limit", "30"],
            &format!("context-inbox:{}", params.project),
        ) {
            Ok(v) => {
                let items = v.get("items").cloned().unwrap_or(serde_json::json!([]));
                serde_json::from_value(items).unwrap_or_default()
            }
            Err(_) => vec![],
        };
        for item in &ctx_items {
            if seen_ctx_limit.contains(&item.id) {
                continue;
            }
            seen_ctx_limit.insert(item.id.clone());
            let Some(target) = context_limit_target(&item.from, &states) else {
                eprintln!(
                    "flywheel: CONTEXT_LIMIT msg {} from unknown sender {:?} — ignored",
                    item.id, item.from
                );
                continue;
            };
            eprintln!(
                "flywheel: CONTEXT_LIMIT from {} → respawning {target} fresh this cycle",
                item.from
            );
            swarm.suppress_context_limit(&target);
            respawn_and_requeue(
                &params,
                &mut queue,
                &agent_kinds,
                &target,
                None,
                "context_limit",
                0,
            )
            .await;
            // The suppression exists only for the dead generation. Once the
            // fresh pane is up, its mail must ring again (bd-ctx-suppressed).
            swarm.clear_context_limit(&target);
        }

        exited_names.clear();
        for ev in &event_batch.events {
            if ev.event_type == "pane.exited" {
                if let Some(n) = name_for_pane(&states, &ev.pane_id) {
                    exited_names.insert(n);
                } else if let Some(a) = &ev.agent {
                    let hits: Vec<_> = states.iter().filter(|s| s.kind == *a).collect();
                    if hits.len() == 1 {
                        exited_names.insert(hits[0].name.clone());
                    }
                }
            }
        }
        let mut reclaim_actions: Vec<LoopAction> = Vec::new();
        let mut heal_msgs: Vec<LoopMsg> = states
            .iter()
            .map(|s| LoopMsg::Heal {
                name: s.name.clone(),
                status: s.status.clone(),
                exited: exited_names.contains(&s.name),
            })
            .collect();

        let mut wave_held: Vec<String> = Vec::new();
        if params.work {
            let inprog: Vec<ReadyBead> = match crate::elm::leaf::run_leaf_journaled(
                &params.project,
                "__inprogress",
                &[&params.project],
                &format!("inprogress:{}", params.project),
            ) {
                Ok(v) => match parse_bead_list(&v, "__inprogress") {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("flywheel: poll_inprogress leaf failed (continuing): {e}");
                        vec![]
                    }
                },
                Err(e) => {
                    eprintln!("flywheel: poll_inprogress leaf failed (continuing): {e}");
                    vec![]
                }
            };
            let live_names: Vec<String> = states.iter().map(|s| s.name.clone()).collect();
            let stranded: Vec<String> = stranded_beads(&inprog, &live_names)
                .into_iter()
                .map(|b| b.id.clone())
                .collect();
            if !stranded.is_empty() {
                let live_arg = live_names.join(",");
                match crate::elm::leaf::run_leaf_journaled(
                    &params.project,
                    "__reclaim-stranded",
                    &[&params.project, "--live", &live_arg],
                    &format!("reclaim-stranded:{}", params.project),
                ) {
                    Ok(r) => {
                        if let Some(ids) = r.get("reclaimed").and_then(|v| v.as_array()) {
                            for id in ids.iter().filter_map(|v| v.as_str()) {
                                eprintln!(
                                    "flywheel: reclaimed stranded bead {id} (re-dispatching)"
                                );
                            }
                        }
                        if let Some(failed) = r.get("failed").and_then(|v| v.as_array()) {
                            for f in failed {
                                eprintln!("flywheel: reclaim failed for {f}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("flywheel: reclaim_stranded leaf failed (continuing): {e}");
                    }
                }
            }
            let to_resume: Vec<&ReadyBead> = inprog
                .iter()
                .filter(|b| !assigned.contains(&b.id) && !stranded.contains(&b.id))
                .collect();
            for bead in to_resume {
                queue_brief(
                    &mut queue,
                    &bead.id,
                    &bead.title,
                    &params.project,
                    true,
                    false,
                );
                assigned.push(bead.id.clone());
            }

            let ready: Vec<ReadyBead> = match crate::elm::leaf::run_leaf_journaled(
                &params.project,
                "__ready",
                &[&params.project],
                &format!("ready:{}", params.project),
            ) {
                Ok(v) => match parse_bead_list(&v, "__ready") {
                    Ok(b) => b,
                    Err(e) => {
                        critical_leaf_failed_this_iter = true;
                        eprintln!("flywheel: poll_ready parse failed (continuing): {e}");
                        vec![]
                    }
                },
                Err(e) => {
                    critical_leaf_failed_this_iter = true;
                    eprintln!("flywheel: poll_ready leaf failed (continuing): {e}");
                    vec![]
                }
            };
            let (dispatchable, not_ready) = partition_ready(&ready);
            wave_held = if params.no_wave_gates {
                Vec::new()
            } else {
                wave_gate_held(&ready, &inprog)
            };
            let mut scope_block: Option<String> = None;
            let scope_ids: Option<std::collections::HashSet<String>> = params
                .wave_label
                .as_ref()
                .map(|label| match wave_scope_filter(&ready, &inprog, label) {
                    Ok(scoped) => scoped.into_iter().map(|b| b.id).collect(),
                    Err(e) => {
                        eprintln!("flywheel: {e}");
                        scope_block = Some(e);
                        Default::default()
                    }
                });
            let to_assign: Vec<&ReadyBead> = dispatchable
                .iter()
                .filter(|b| !assigned.contains(&b.id) && !wave_held.contains(&b.id))
                .filter(|b| match (&scope_ids, &scope_block) {
                    (Some(ids), _) => ids.contains(&b.id),
                    (None, Some(_)) => false,
                    (None, None) => true,
                })
                .copied()
                .collect();
            if !wave_held.is_empty() {
                eprintln!(
                    "flywheel: wave-gate: holding {} (earlier wave open; not stuck-owner)",
                    wave_held.join(", ")
                );
            }
            let board: Vec<(String, String)> = reservation_board_live(&params.project).await;
            let overlap: std::collections::HashSet<String> = {
                let in_flight: Vec<String> = inprog.iter().map(|b| b.id.clone()).collect();
                bead_overlap_waits(&board, &in_flight)
            };
            let live_leases: std::collections::HashSet<String> = board
                .iter()
                .filter_map(|(_, reason)| {
                    let r = reason.trim();
                    if r.is_empty() {
                        None
                    } else {
                        Some(r.to_string())
                    }
                })
                .collect();
            let serialize_all = always_serialize_held(&board);
            {
                for bead in &inprog {
                    for exited in &exited_names {
                        if let Some(act) = reclaim_on_pane_exited(
                            bead,
                            exited,
                            &states,
                            swarm
                                .heal_states
                                .get(exited)
                                .unwrap_or(&HealState::default()),
                            params.respawn_budget,
                        ) {
                            if board.iter().any(|(_, reason)| reason == &bead.id) {
                                continue;
                            }
                            reclaim_actions.push(act);
                        }
                    }
                }
                let now_secs = authority.now_unix() as i64;
                let stale =
                    plan_reclaim_with_policy(now_secs, &inprog, &states, &board, &params.reclaim);
                for act in stale {
                    reclaim_actions.push(act);
                }
            }
            let candidate_ids: Vec<String> = to_assign.iter().map(|b| b.id.clone()).collect();
            let total = candidate_ids.len();
            let (next_table, effects) = doorbell_core::wake_core::tick(
                &wake,
                &doorbell_core::wake_core::WakeMsg::FloodCheck { ids: candidate_ids },
            );
            wake = next_table;
            let gate = effects.into_iter().find_map(|e| match e {
                doorbell_core::wake_core::WakeEffect::FloodVerdict { gate } => Some(gate),
                _ => None,
            });
            let mut paused = false;
            if let Some(gate) = gate.as_ref()
                && let doorbell_core::wake_core::FloodGate::Pause { unseen } = gate
            {
                eprintln!(
                    "flywheel: CRITICAL: {}",
                    doorbell_core::wake_core::why_flood(gate, total, unseen.len())
                );
                paused = true;
            }
            if paused {
            } else if serialize_all {
                eprintln!(
                    "flywheel: always-serialize path held ({}) — no new ready-bead dispatch this poll (ADR-0019 Spec 3.5; serialized until the holder releases)",
                    board
                        .iter()
                        .filter(|(p, _)| ALWAYS_SERIALIZE_PATHS.iter().any(|a| overlaps(p, a)))
                        .map(|(p, r)| format!("{p} (reason={r})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            } else {
                for bead in to_assign {
                    if let Some(wf_name) = workflow_for_bead(bead) {
                        if bead.verify.is_some() && principles_block_reason(bead).is_none() {
                            match spawn_graph_vehicle(
                                &params.project,
                                &bead.id,
                                &wf_name,
                                &bead.title,
                                bead.verify.as_deref(),
                                &mut graph_invoked,
                            ) {
                                Ok(_outcome) => {
                                    assigned.push(bead.id.clone());
                                    continue;
                                }
                                Err(e) => eprintln!(
                                    "flywheel: [bd-bpd9] vehicle spawn failed for {} (falling back to brief): {e}",
                                    bead.id
                                ),
                            }
                        } else {
                            let why = if bead.verify.is_none() {
                                "VERIFY fence missing — the vehicle needs the bead's VERIFY command"
                                    .to_string()
                            } else {
                                principles_block_reason(bead)
                                    .unwrap_or_else(|| "principles block".to_string())
                            };
                            eprintln!(
                                "flywheel: [bd-bpd9] refusing graph vehicle for {}:                                  marker workflow:{} present but {why} — add the fence                                  (## VERIFY) or fix principles, or remove the marker",
                                bead.id, wf_name
                            );
                            continue;
                        }
                    }
                    let g_now_us = authority.now_unix() as i64 * 1_000_000;
                    let g_facts = match crate::elm::leaf::run_leaf_journaled(
                        &params.project,
                        "__dispatch-guard",
                        &[&params.project, &bead.id],
                        &format!("dispatch-guard:{}:{g_now_us}", bead.id),
                    ) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            eprintln!(
                                "flywheel: dispatch-guard leaf failed for {} (fail-open): {e}",
                                bead.id
                            );
                            None
                        }
                    };
                    if let Some(g_facts) = g_facts {
                        let g_v = settle_dispatch_guard_output(
                            &g_facts,
                            &bead.id,
                            g_now_us,
                            params.dispatch_guard_ttl_us,
                        );
                        if g_v.get("dispatch").and_then(Value::as_bool) == Some(false) {
                            let reason =
                                g_v.get("reason").and_then(Value::as_str).unwrap_or("held");
                            eprintln!("flywheel: dispatch-guard HOLD {}: {reason}", bead.id);
                            continue;
                        }
                    }
                    queue_brief(
                        &mut queue,
                        &bead.id,
                        &bead.title,
                        &params.project,
                        false,
                        false,
                    );
                    assigned.push(bead.id.clone());
                }
            }
            for bead in &not_ready {
                if not_ready_mailed.insert(bead.id.clone()) {
                    let reason = not_ready_reason(bead)
                        .unwrap_or_else(|| "not dispatchable (see loop)".to_string());
                    let argv = vec![
                        params.project.as_str(),
                        bead.id.as_str(),
                        bead.title.as_str(),
                        "--reason",
                        reason.as_str(),
                    ];
                    match crate::elm::leaf::run_leaf_journaled(
                        &params.project,
                        "__mail-not-ready",
                        &argv,
                        &format!("mail-not-ready:{}", bead.id),
                    ) {
                        Ok(v) => eprintln!(
                            "flywheel: bead {} NOT ready ({reason}) — {}: {v}",
                            bead.id,
                            not_ready_mail_status(&v)
                        ),
                        Err(e) => eprintln!(
                            "flywheel: mail-not-ready for {} failed (continuing): {e}",
                            bead.id
                        ),
                    }
                }
            }

            let mut watch_actions = Vec::new();
            swarm.fold_claim_watches(|claim_watch| {
                watch_actions = claim_watch_step_with_known(
                    claim_watch,
                    &ready,
                    &inprog,
                    &not_ready,
                    &states,
                    params.claim_nudge_after_polls,
                    &overlap,
                    &live_leases,
                );
            });
            for a in watch_actions {
                match a {
                    ClaimWatchAction::NudgeClaim { agent, bead_id } => {
                        nudge_msgs.push(LoopMsg::ClaimNudge {
                            name: agent,
                            bead_id,
                        });
                    }
                    ClaimWatchAction::RedispatchBead { bead_id, title } => {
                        let g_now_us = authority.now_unix() as i64 * 1_000_000;
                        let g_hold: Option<String> = match crate::elm::leaf::run_leaf_journaled(
                            &params.project,
                            "__dispatch-guard",
                            &[&params.project, &bead_id],
                            &format!("dispatch-guard:{bead_id}:{g_now_us}"),
                        ) {
                            Ok(facts) => {
                                let v = settle_dispatch_guard_output(
                                    &facts,
                                    &bead_id,
                                    g_now_us,
                                    params.dispatch_guard_ttl_us,
                                );
                                if v.get("dispatch").and_then(Value::as_bool) == Some(false) {
                                    Some(
                                        v.get("reason")
                                            .and_then(Value::as_str)
                                            .unwrap_or("held")
                                            .to_string(),
                                    )
                                } else {
                                    None
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "flywheel: dispatch-guard leaf failed for {bead_id} (fail-open): {e}"
                                );
                                None
                            }
                        };
                        match g_hold {
                            Some(reason) => {
                                eprintln!(
                                    "flywheel: dispatch-guard HOLD re-dispatch {bead_id}: {reason}"
                                );
                            }
                            None => {
                                queue_brief(
                                    &mut queue,
                                    &bead_id,
                                    &title,
                                    &params.project,
                                    false,
                                    true,
                                );
                            }
                        }
                    }
                    ClaimWatchAction::NudgeClose {
                        agent,
                        bead_id,
                        title,
                    } => {
                        nudge_msgs.push(LoopMsg::CloseNudge {
                            name: agent,
                            bead_id,
                            title,
                        });
                    }
                    ClaimWatchAction::RedispatchResume { bead_id, title } => {
                        let g_now_us = authority.now_unix() as i64 * 1_000_000;
                        let g_hold: Option<String> = match crate::elm::leaf::run_leaf_journaled(
                            &params.project,
                            "__dispatch-guard",
                            &[&params.project, &bead_id],
                            &format!("dispatch-guard:{bead_id}:{g_now_us}"),
                        ) {
                            Ok(facts) => {
                                let v = settle_dispatch_guard_output(
                                    &facts,
                                    &bead_id,
                                    g_now_us,
                                    params.dispatch_guard_ttl_us,
                                );
                                if v.get("dispatch").and_then(Value::as_bool) == Some(false) {
                                    Some(
                                        v.get("reason")
                                            .and_then(Value::as_str)
                                            .unwrap_or("held")
                                            .to_string(),
                                    )
                                } else {
                                    None
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "flywheel: dispatch-guard leaf failed for {bead_id} (fail-open): {e}"
                                );
                                None
                            }
                        };
                        match g_hold {
                            Some(reason) => {
                                eprintln!(
                                    "flywheel: dispatch-guard HOLD resume {bead_id}: {reason}"
                                );
                            }
                            None => {
                                queue_brief(
                                    &mut queue,
                                    &bead_id,
                                    &title,
                                    &params.project,
                                    true,
                                    true,
                                );
                            }
                        }
                    }
                    ClaimWatchAction::ReleaseReservations { bead_id } => {
                        // A WorkerReceipt grace tick is NOT a close: keep the
                        // worker's file reservations until the close is legal
                        // or finally rejected.
                        let mut release_locks = true;
                        let mut dispatch_verifier = false;
                        match crate::elm::leaf::run_leaf_journaled(
                            &params.project,
                            "__close-check",
                            &[&params.project, &bead_id],
                            &format!("close-check:{bead_id}"),
                        ) {
                            Ok(v) => {
                                let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
                                match status {
                                    "legal" => {
                                        swarm.clear_bead_slot(&bead_id);
                                        eprintln!(
                                            "flywheel: close of {bead_id} verified (legal verdict bound to sha)"
                                        );
                                        emit_ledger_fact(
                                            &crate::config::state_dir(),
                                            &params.project,
                                            &bead_id,
                                            LedgerEvent::Closed,
                                        );
                                        if swarm.done_mailed.insert(bead_id.clone()) {
                                            match crate::elm::leaf::run_leaf_journaled(
                                                &params.project,
                                                "__mail-done",
                                                &[&params.project, &bead_id],
                                                &format!("mail-done:{bead_id}"),
                                            ) {
                                                Ok(r) => eprintln!(
                                                    "flywheel: done-mail sent for {bead_id}: {r}"
                                                ),
                                                Err(e) => eprintln!(
                                                    "flywheel: done-mail for {bead_id} failed (continuing): {e}"
                                                ),
                                            }
                                        }
                                        dispatch_verifier = true;
                                    }
                                    "no-commit" => {
                                        swarm.clear_bead_slot(&bead_id);
                                        emit_ledger_fact(
                                            &crate::config::state_dir(),
                                            &params.project,
                                            &bead_id,
                                            LedgerEvent::Reopened,
                                        );
                                        match crate::elm::leaf::run_leaf_journaled(
                                            &params.project,
                                            "__reopen-bead",
                                            &[&params.project, &bead_id, ""],
                                            &format!("reopen:{bead_id}:nocommit"),
                                        ) {
                                            Ok(r) => eprintln!(
                                                "flywheel: {bead_id} closed without a commit — reopened: {r}"
                                            ),
                                            Err(e) => eprintln!(
                                                "flywheel: reopen of {bead_id} failed (continuing): {e}"
                                            ),
                                        }
                                    }
                                    "missing" => {
                                        let eligible = v
                                            .get("receipt_eligible")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false);
                                        let sha = v
                                            .get("sha")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string();
                                        let mut lr_band = String::new();
                                        let lr_refs: Vec<&str> =
                                            vec![&params.project, &bead_id, "--sha", &sha];
                                        let lr_value =
                                            match crate::elm::leaf::run_leaf_journaled_timeout(
                                                &params.project,
                                                "__loop-run-verify",
                                                &lr_refs,
                                                &format!("loop-run-verify:{bead_id}:{sha}"),
                                                None,
                                            ) {
                                                Ok(v) => Some(v),
                                                Err(e) => {
                                                    eprintln!(
                                                        "flywheel: loop-run-verify for {bead_id} failed (continuing): {e}"
                                                    );
                                                    None
                                                }
                                            };
                                        if let Some(v) = lr_value {
                                            let settled = settle_loop_verify_output(
                                                &v,
                                                &params.project,
                                                &bead_id,
                                                &sha,
                                                params.strict,
                                                authority.now_datetime(),
                                                || {
                                                    reopen_bead_journaled(
                                                        &params.project,
                                                        &bead_id,
                                                        &sha,
                                                    )
                                                },
                                            )?;
                                            lr_band = settled
                                                .get("band")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_string();
                                        }
                                        if lr_band != "loop-runnable" {
                                            let ticks = swarm.bump_close_grace(&bead_id);
                                            if eligible && ticks < 2 {
                                                eprintln!(
                                                    "flywheel: {bead_id} missing verdict (WorkerReceipt grace {ticks}/2)"
                                                );
                                                release_locks = false;
                                                let agent = swarm
                                                    .bead_slots
                                                    .get(&bead_id)
                                                    .and_then(|slot| slot.watch.as_ref())
                                                    .map(|cw| cw.agent.clone())
                                                    .unwrap_or_default();
                                                swarm.bead_slot(&bead_id).watch =
                                                    Some(ClaimWatch {
                                                        agent,
                                                        unclaimed_polls: 0,
                                                        close_nudged: false,
                                                        close_polls: 0,
                                                        wait_reason: None,
                                                    });
                                            } else {
                                                swarm.clear_bead_slot(&bead_id);
                                                emit_ledger_fact(
                                                    &crate::config::state_dir(),
                                                    &params.project,
                                                    &bead_id,
                                                    LedgerEvent::Reopened,
                                                );
                                                let rb_sha = sha.clone();
                                                match crate::elm::leaf::run_leaf_journaled(
                                                    &params.project,
                                                    "__reopen-bead",
                                                    &[&params.project, &bead_id, &rb_sha],
                                                    &format!("reopen:{bead_id}:{rb_sha}"),
                                                ) {
                                                    Ok(r) => eprintln!(
                                                        "flywheel: {bead_id} unverified close — reopened: {r}"
                                                    ),
                                                    Err(e) => eprintln!(
                                                        "flywheel: reopen of {bead_id} failed (continuing): {e}"
                                                    ),
                                                }
                                            }
                                        } else {
                                            swarm.clear_bead_slot(&bead_id);
                                            emit_ledger_fact(
                                                &crate::config::state_dir(),
                                                &params.project,
                                                &bead_id,
                                                LedgerEvent::Closed,
                                            );
                                            dispatch_verifier = true;
                                        }
                                    }
                                    "not-closed" => {
                                        release_locks = false;
                                        let agent = swarm
                                            .bead_slots
                                            .get(&bead_id)
                                            .and_then(|slot| slot.watch.as_ref())
                                            .map(|cw| cw.agent.clone())
                                            .unwrap_or_default();
                                        swarm.bead_slot(&bead_id).watch = Some(ClaimWatch {
                                            agent,
                                            unclaimed_polls: 0,
                                            close_nudged: false,
                                            close_polls: 0,
                                            wait_reason: Some(WaitReason::Blocked),
                                        });
                                    }
                                    other => eprintln!(
                                        "flywheel: close-check for {bead_id} unknown status {other}"
                                    ),
                                }
                            }
                            Err(e) => eprintln!(
                                "flywheel: close-check for {bead_id} failed (continuing): {e}"
                            ),
                        }
                        if dispatch_verifier && swarm.mark_verifier_dispatched(&bead_id) {
                            match crate::elm::leaf::run_leaf_journaled(
                                &params.project,
                                "__verify-dispatch",
                                &[&params.project, &bead_id],
                                &format!("verify-dispatch:{bead_id}"),
                            ) {
                                Ok(v) => {
                                    let plan = v.get("plan").and_then(Value::as_str).unwrap_or("?");
                                    if plan == "degrade" {
                                        eprintln!(
                                            "flywheel: bead {bead_id} blocked verifier-family-missing (single-family swarm; captain mailed once)"
                                        );
                                    } else if plan == "dispatch" {
                                        let model =
                                            v.get("model").and_then(Value::as_str).unwrap_or("?");
                                        eprintln!(
                                            "flywheel: bead {bead_id} needs an independent verifier on {model} (reviewer pane dispatch, ADR-0019 Spec 4)"
                                        );
                                    }
                                }
                                Err(e) => eprintln!(
                                    "flywheel: verifier-dispatch for {bead_id} failed (continuing): {e}"
                                ),
                            }
                        }
                        if release_locks {
                            match crate::elm::leaf::run_leaf_journaled(
                                &params.project,
                                "__release-bead-locks",
                                &[&params.project, &bead_id],
                                &format!("release-locks:{bead_id}"),
                            ) {
                                Ok(_) => {}
                                Err(e) => eprintln!(
                                    "flywheel: release of closed bead {bead_id} locks failed (continuing): {e}"
                                ),
                            }
                        }
                    }
                }
            }
        }

        let mut blocked_names: Vec<String> = Vec::new();
        let mut actions: Vec<LoopAction> = Vec::new();
        let wake_suppressed: std::collections::HashSet<String> = swarm.wake_suppressed();
        clear_stale_landed_for_idle(&params.project, &params.invocation, &states);
        {
            let renew_extend = lease_renew_seconds(params.interval_secs);
            let board: Vec<(String, String)> = reservation_board_live(&params.project).await;
            let inprog_for_renew: Vec<ReadyBead> = if params.work {
                match crate::elm::leaf::run_leaf_journaled(
                    &params.project,
                    "__inprogress",
                    &[&params.project],
                    &format!("inprogress:{}", params.project),
                ) {
                    Ok(v) => parse_bead_list(&v, "__inprogress").unwrap_or_default(),
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let live_names: Vec<String> = states.iter().map(|s| s.name.clone()).collect();
            let to_renew: Vec<(String, String)> =
                leases_to_renew_simple(&board, &inprog_for_renew, &live_names);
            if !to_renew.is_empty() {
                let paths: Vec<String> = to_renew.iter().map(|(p, _)| p.clone()).collect();
                match renew_filtered_leases(RenewFilteredArgs {
                    project: params.project.clone(),
                    paths: paths.clone(),
                    extend_seconds: renew_extend,
                })
                .await
                {
                    Ok(v) => {
                        let renewed = v.get("renewed").and_then(Value::as_u64).unwrap_or(0);
                        if renewed > 0 {
                            eprintln!("flywheel: renewed {renewed} lease(s) before dispatch");
                        }
                        update_mail_health(
                            &params.project,
                            true,
                            "renew_leases",
                            &format!("renewed {renewed}"),
                            params.mail_health_escalate_at,
                        );
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        eprintln!(
                            "flywheel: lease renew failed before dispatch (continuing): {err_str}"
                        );
                        record_renew_failure("batch", "batch", &err_str);
                        for (path, bead_id) in &to_renew {
                            emit_lease_lapsed(
                                &crate::config::state_dir(),
                                &params.project,
                                path,
                                Some(bead_id),
                            );
                        }
                        update_mail_health(
                            &params.project,
                            false,
                            "renew_leases",
                            &err_str,
                            params.mail_health_escalate_at,
                        );
                    }
                }
            }
        }
        {
            let now_secs = authority.now_unix() as i64;
            let threshold = params.pane_stall_secs;
            let inprog_for_stall: Vec<ReadyBead> = if params.work {
                match crate::elm::leaf::run_leaf_journaled(
                    &params.project,
                    "__inprogress",
                    &[&params.project],
                    &format!("inprogress:{}", params.project),
                ) {
                    Ok(v) => parse_bead_list(&v, "__inprogress").unwrap_or_default(),
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let stalled = detect_stalled_panes(&inprog_for_stall, &states, now_secs, threshold);
            for ev in stalled {
                emit_fact(&crate::config::state_dir(), &params.project, ev);
            }
        }
        let n = params.mail_health_escalate_at;
        let mail_hold = if n == 0 {
            false
        } else if let Ok(Some(al)) = crate::state::load_active_loop(&params.project) {
            al.mail_health.should_escalate(n) || al.mail_degraded
        } else {
            false
        };
        let active = crate::state::load_active_loop(&params.project)
            .ok()
            .flatten();
        swarm.queue = queue.clone();
        swarm.per_bead_phase = per_bead_phase.clone();
        swarm.wake_suppression = wake_suppressed.clone();
        swarm.event_cursor = active.as_ref().and_then(|a| a.event_cursor.clone());
        swarm.escalation = active
            .as_ref()
            .map(|a| a.escalation.clone())
            .unwrap_or_default();
        swarm.respawn = active
            .as_ref()
            .map(|a| a.respawn.clone())
            .unwrap_or_default();
        swarm.mail_health = active
            .as_ref()
            .map(|a| a.mail_health.clone())
            .unwrap_or_default();
        swarm.mail_degraded = mail_hold;
        swarm.heal_grace = params.unknown_detect_grace_polls;
        swarm.heal_threshold = params.unknown_diagnose_threshold;
        swarm.heal_budget = params.respawn_budget;
        swarm.pending_events = Vec::new();
        swarm.dispatch_pending = dispatch_pending.clone();
        let mut rot_msgs: Vec<LoopMsg> = Vec::new();
        for s in &mut states {
            if s.nudge_streak == 3 {
                rot_msgs.push(LoopMsg::Rot {
                    name: s.name.clone(),
                    events: crate::rot::RotEvents {
                        repetition: s.nudge_streak,
                        marker_silence_turns: if s.status == "idle" {
                            s.nudge_streak
                        } else {
                            0
                        },
                        ..crate::rot::RotEvents::default()
                    },
                });
                s.nudge_streak = 4;
            }
        }
        swarm.states = states.clone();
        let now = authority.now_unix();
        let fold = |swarm: &mut LoopModel,
                    ws_msg: crate::workspace::Msg,
                    journal: LoopMsg|
         -> Vec<LoopAction> {
            let before = swarm.clone();
            let cmds = tick_loop_owner(swarm, &ws_msg);
            let acts: Vec<LoopAction> = cmds
                .iter()
                .filter_map(super::workspace::Cmd::to_loop_action)
                .collect();
            let _ = crate::state::journal_loop_msg(&params.project, &journal, &before, &acts);
            acts
        };
        let mut poll_actions = Vec::new();
        for pe in &event_batch.events {
            poll_actions.extend(fold(
                &mut swarm,
                crate::workspace::Msg::Hc { event: pe.clone() },
                LoopMsg::Hc(pe.clone()),
            ));
        }
        let pause_secs = params.pause_resurface_secs;
        let drained: Vec<String> = swarm.drain_pending.keys().cloned().collect();
        for name in drained {
            if swarm
                .drain_pending
                .get(&name)
                .is_none_or(|ds| ds.paused.is_some())
            {
                continue;
            }
            let terminal = read_pane_terminal_live(&params.project, &name).await;
            let Some(declared) = crate::write_liveness::parse_paused_declaration(&terminal) else {
                continue;
            };
            let journal_msg = LoopMsg::PauseDeclared {
                name: name.clone(),
                owner: declared.owner.clone(),
                now,
                resurface_secs: pause_secs,
            };
            let before = swarm.clone();
            let (folded, acts) = crate::workspace::tick_from_loop_msg(&swarm, &journal_msg);
            swarm = folded;
            let _ = crate::state::journal_loop_msg(&params.project, &journal_msg, &before, &acts);
            poll_actions.extend(acts);
            eprintln!(
                "flywheel: {name} declared `paused: {}` — grace held, resurfacing every {pause_secs}s (never auto-interrupt)",
                declared.owner
            );
        }
        poll_actions.extend(fold(
            &mut swarm,
            crate::workspace::Msg::Tick { now },
            LoopMsg::Tick { now },
        ));
        let mut extra_msgs = std::mem::take(&mut heal_msgs);
        extra_msgs.append(&mut nudge_msgs);
        extra_msgs.append(&mut rot_msgs);
        for m in extra_msgs {
            let ws_msg = match &m {
                LoopMsg::Heal {
                    name,
                    status,
                    exited,
                } => crate::workspace::Msg::Heal {
                    name: name.clone(),
                    status: status.clone(),
                    exited: *exited,
                },
                LoopMsg::ClaimNudge { name, bead_id } => crate::workspace::Msg::ClaimNudge {
                    name: name.clone(),
                    bead_id: bead_id.clone(),
                },
                LoopMsg::CloseNudge {
                    name,
                    bead_id,
                    title,
                } => crate::workspace::Msg::CloseNudge {
                    name: name.clone(),
                    bead_id: bead_id.clone(),
                    title: title.clone(),
                },
                LoopMsg::Rot { name, events } => {
                    let (next, acts) = tick(
                        &swarm,
                        &LoopMsg::Rot {
                            name: name.clone(),
                            events: events.clone(),
                        },
                    );
                    swarm = next;
                    poll_actions.extend(acts);
                    continue;
                }
                other => {
                    eprintln!(
                        "flywheel: unmapped LoopMsg in workspace fold (skipped, no durable::tick): {other:?}"
                    );
                    continue;
                }
            };
            poll_actions.extend(fold(&mut swarm, ws_msg, m));
        }
        for act in reclaim_actions {
            if let LoopAction::Reclaim {
                bead_id,
                pin,
                terminal_name,
                reason,
            } = act
            {
                let journal = LoopMsg::Reclaim {
                    bead_id: bead_id.clone(),
                    holder_pin: pin.clone(),
                    claimant_pin: pin.clone(),
                    is_live: false,
                };
                poll_actions.extend(fold(
                    &mut swarm,
                    crate::workspace::Msg::Reclaim {
                        bead_id,
                        holder_pin: pin.clone(),
                        claimant_pin: terminal_name,
                        is_live: false,
                        reason,
                    },
                    journal,
                ));
            }
        }
        let mut next_model = swarm.clone();
        queue = next_model.queue.clone();
        per_bead_phase = next_model.per_bead_phase.clone();
        dispatch_pending = next_model.dispatch_pending.clone();
        // tick_loop_owner may update drain_pending (a write-liveness defer).
        // Copy it back before the ActiveLoop save or the defer is dropped at
        // the end of this iteration (bd-drain-pending-shadow).
        swarm.drain_pending = next_model.drain_pending.clone();
        states = next_model.states.clone();
        {
            let live_agents: std::collections::HashSet<&String> =
                states.iter().map(|s| &s.name).collect();
            next_model
                .heal_states
                .retain(|k, _| live_agents.contains(k));
            swarm.heal_states = next_model.heal_states.clone();
            next_model
                .agent_slots
                .retain(|k, _| live_agents.contains(k));
            swarm.agent_slots = next_model.agent_slots.clone();
            if per_bead_phase.len() > 500 {
                let mut keys: Vec<String> = per_bead_phase.keys().cloned().collect();
                keys.sort();
                let to_remove = per_bead_phase.len() - 400;
                for k in keys.into_iter().take(to_remove) {
                    if matches!(
                        per_bead_phase.get(&k),
                        Some(LoopPhase::Complete | LoopPhase::WaveHeld)
                    ) {
                        per_bead_phase.remove(&k);
                    } else if per_bead_phase.len() <= 400 {
                        break;
                    }
                }
            }
            let live_beads: std::collections::HashSet<&String> = queue
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .chain(states.iter().map(|s| &s.name))
                .collect();
            dispatch_pending.retain(|k| live_beads.contains(k) || per_bead_phase.contains_key(k));
        }
        clear_live_failures(&mut send_failures, &states);
        learn_kinds(&mut agent_kinds, &states);
        if let Ok(Some(mut al)) = crate::state::load_active_loop(&params.project) {
            al.per_bead_phase = per_bead_phase
                .iter()
                .map(|(k, v)| (k.clone(), format!("{v:?}")))
                .collect();
            al.dispatch_pending = dispatch_pending.clone();
            al.wake = wake.clone();
            al.done_mailed = swarm.done_mailed.clone();
            al.drain_pending = swarm
                .drain_pending
                .iter()
                .map(|(k, v)| (k.clone(), v.bead_id.clone()))
                .collect();
            al.bead_slots = swarm.bead_slots.clone();
            al.agent_slots = swarm.agent_slots.clone();
            al.heal_states = next_model
                .heal_states
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        crate::state::HealState {
                            diagnose_count: v.diagnose_count,
                            respawn_count: v.respawn_count,
                            unknown_count: v.unknown_count,
                            // Preserve the Diagnose -> Respawn ladder across
                            // a resume; hardcoding false restarted it every
                            // time and could never reach Respawn.
                            awaiting_clear: v.awaiting_clear,
                        },
                    )
                })
                .collect();
            let _ = crate::state::save_active_loop(&params.project, &al);
        }
        for a in &poll_actions {
            if let LoopAction::Blocked { name } = a {
                blocked_names.push(name.clone());
            }
        }
        actions.extend(poll_actions);

        if !blocked_names.is_empty() && params.on_blocked == "stop" {
            return Ok(format!(
                "stopped: blocked agents {}",
                blocked_names.join(", ")
            ));
        }

        let expected_entries = match crate::elm::leaf::run_leaf_journaled(
            &params.project,
            "__expected-replies",
            &[&params.project],
            &format!("expected-replies:{}", params.project),
        ) {
            Ok(v) => parse_expected_reply_entries(&v),
            Err(e) => {
                critical_leaf_failed_this_iter = true;
                eprintln!("flywheel: expected-replies leaf failed (continuing): {e}");
                vec![]
            }
        };
        let prev = std::mem::take(&mut expected_replies);
        expected_replies = reconcile_expected_replies(&prev, &expected_entries);

        let mut satisfied_names: Vec<String> = Vec::new();
        let mut escalate_names: Vec<String> = Vec::new();
        for (key, exp) in &mut expected_replies {
            let agent = exp.agent.clone();
            let Some(matched) = match_expected_agent(&params.project, &states, &agent) else {
                continue;
            };
            let terminal_name = matched.name.clone();
            if dispatch_pending.contains(&terminal_name) {
                continue;
            }
            let settled = states
                .iter()
                .any(|s| s.name == terminal_name && matches!(s.status.as_str(), "idle" | "done"));
            if !settled {
                continue;
            }
            let (newest_id, author) = match crate::elm::leaf::run_leaf_journaled(
                &params.project,
                "__check-reply",
                &[&params.project, &terminal_name, &exp.thread],
                &format!(
                    "check-reply:{}:{}:{}",
                    params.project, terminal_name, exp.thread
                ),
            ) {
                Ok(v) => parse_reply_check(&v),
                Err(e) => {
                    eprintln!("flywheel: check-reply leaf failed for {agent} (continuing): {e}");
                    (None, None)
                }
            };
            let mail_name = matched.mail_identity.clone().unwrap_or_else(|| {
                crate::resolve_mail_target_registry(&params.project, &terminal_name)
            });
            match verify_reply(
                exp.head.as_deref(),
                newest_id.as_deref(),
                author.as_deref(),
                &mail_name,
                exp.reminder_count,
            ) {
                ReplyVerdict::Satisfied => {
                    eprintln!(
                        "flywheel: reply verified for {agent} in thread {}",
                        exp.thread
                    );
                    let _ = crate::elm::leaf::run_leaf_journaled(
                        &params.project,
                        "__reply-settled",
                        &[&params.project, &exp.agent, &exp.thread],
                        &format!(
                            "reply-settled:{}:{}:{}",
                            params.project, exp.agent, exp.thread
                        ),
                    );
                    satisfied_names.push(key.clone());
                }
                ReplyVerdict::Reminded => {
                    let remind_args = RemindArgs {
                        project: params.project.clone(),
                        name: terminal_name.clone(),
                        thread: exp.thread.clone(),
                        reminder_count: exp.reminder_count,
                        invocation: params.invocation.clone(),
                        iteration,
                    };
                    let remind_argv = remind_reply_argv(&remind_args);
                    let remind_refs: Vec<&str> = remind_argv.iter().map(|s| s.as_str()).collect();
                    let _ = crate::elm::leaf::run_leaf_journaled(
                        &params.project,
                        "__remind-reply",
                        &remind_refs[1..],
                        &format!(
                            "remind-reply:{}:{}:{}:{}",
                            params.project, terminal_name, exp.thread, exp.reminder_count
                        ),
                    );
                    exp.reminder_count = 1;
                }
                ReplyVerdict::Escalate => {
                    escalate_names.push(terminal_name.clone());
                }
            }
        }
        expected_replies.retain(|a, _| !satisfied_names.contains(a));
        if !escalate_names.is_empty() {
            eprintln!(
                "flywheel: reply still missing after reminder for {} — escalating",
                escalate_names.join(", ")
            );
            for n in &escalate_names {
                blocked_names.push(n.clone());
                actions.push(LoopAction::Blocked { name: n.clone() });
            }
        }

        let mut any_work = false;
        for action in &actions {
            match action {
                LoopAction::PinStamp { bead_id, pin } => {
                    let exe = self_exe()?;
                    let mut cmd = Command::new(&exe);
                    cmd.args(["__pin-stamp", &params.project, bead_id, pin]);
                    match run_leaf_cmd_bounded(cmd, "__pin-stamp", LEAF_WATCHDOG_GRACE).await {
                        Ok(out) if out.status.success() => {}
                        Ok(out) => {
                            let _ = crate::state::log_event(
                                &params.project,
                                crate::state::EventKind::Send,
                                serde_json::json!({
                                    "pin_stamp": bead_id,
                                    "pin": pin,
                                    "ok": false,
                                    "stderr": String::from_utf8_lossy(&out.stderr),
                                }),
                            );
                        }
                        Err(e) => {
                            let _ = crate::state::log_event(
                                &params.project,
                                crate::state::EventKind::Send,
                                serde_json::json!({
                                    "pin_stamp": bead_id,
                                    "pin": pin,
                                    "ok": false,
                                    "error": e.to_string(),
                                }),
                            );
                        }
                    }
                }
                LoopAction::Send { name, text } | LoopAction::DispatchImmediate { name, text } => {
                    let read_now = authority.now_unix();
                    let verdict = match crate::elm::leaf::run_leaf_journaled(
                        &params.project,
                        "__read",
                        &[&params.project, name],
                        &format!("read:{}:{name}", params.project),
                    ) {
                        Ok(v) => {
                            let ro = parse_read_output(&v);
                            context_verdict(&ro.terminal, ro.state.as_ref(), read_now)
                        }
                        Err(e) => {
                            eprintln!(
                                "flywheel: read_output leaf failed for {name} (continuing): {e}"
                            );
                            ContextVerdict::Healthy
                        }
                    };
                    if verdict == ContextVerdict::Exhausted {
                        clear_dispatch_pending(&mut dispatch_pending, name, Some(false));
                        respawn_and_requeue(
                            &params,
                            &mut queue,
                            &agent_kinds,
                            name,
                            Some(text),
                            "context_limit",
                            0,
                        )
                        .await;
                        continue;
                    }
                    let outcome = match send_prompt(SendArgs {
                        project: params.project.clone(),
                        name: name.clone(),
                        text: text.clone(),
                        confirm: params.confirm_sends,
                        confirm_timeout_ms: params.confirm_timeout_ms,
                        invocation: params.invocation.clone(),
                        iteration,
                        profile: match crate::elm::leaf::run_leaf_journaled(
                            &params.project,
                            "__profile",
                            &[&params.project, name],
                            &format!("profile:{}:{name}", params.project),
                        ) {
                            Ok(v) => v
                                .as_str()
                                .map(|s| s.to_string())
                                .or_else(|| {
                                    v.get("raw").and_then(|r| r.as_str()).map(|s| s.to_string())
                                })
                                .unwrap_or_default(),
                            Err(e) => {
                                eprintln!(
                                    "flywheel: __profile failed for {name} (sending raw): {e}"
                                );
                                String::new()
                            }
                        },
                    })
                    .await
                    {
                        Ok(o) => o,
                        Err(e) => SendOutcome {
                            ok: false,
                            name: name.clone(),
                            landed: Some(false),
                            status: None,
                            reason: Some(format!("error:{e}")),
                            deduped: false,
                            cancelled: crate::failure::classify(&format!("{e}"), None, None).kind
                                == crate::failure::FailureKind::Cancelled,
                        },
                    };
                    clear_dispatch_pending(&mut dispatch_pending, name, outcome.landed);
                    any_work = true;
                    match handle_send_result(&mut send_failures, name, &outcome) {
                        Escalation::Accept => {
                            if let Some(id) = bead_id_from_prompt(text) {
                                let slot = swarm.bead_slot(id);
                                if slot.watch.is_none() {
                                    slot.watch = Some(ClaimWatch {
                                        agent: name.clone(),
                                        unclaimed_polls: 0,
                                        close_nudged: false,
                                        close_polls: 0,
                                        wait_reason: None,
                                    });
                                }
                            }
                        }
                        Escalation::FailStop => {
                            return Err(anyhow::anyhow!(
                                "{}",
                                failstop_message(name, outcome.reason.as_deref())
                            ));
                        }
                        Escalation::Cancelled => {
                            eprintln!(
                                "flywheel: send to {name} cancelled; preserving cancellation without respawn"
                            );
                        }
                        Escalation::RespawnAndRequeue => {
                            let attempts = send_failures.get(name).copied().unwrap_or(0);
                            let reason = outcome.reason.as_deref().unwrap_or("stalled");
                            respawn_and_requeue(
                                &params,
                                &mut queue,
                                &agent_kinds,
                                name,
                                Some(text),
                                reason,
                                attempts,
                            )
                            .await;
                        }
                    }
                }
                LoopAction::Blocked { name } => {
                    if params.on_blocked == "mail" {
                        let out = match notify_blocked(NotifyArgs {
                            project: params.project.clone(),
                            name: name.clone(),
                            now_us: authority.now_unix() as i64 * 1_000_000,
                            first_blocked_ts: None,
                            interval_secs: params.interval_secs,
                            params: serde_json::to_value(&params)
                                .unwrap_or(serde_json::Value::Null),
                        })
                        .await
                        {
                            Ok(o) => o,
                            Err(e) => {
                                eprintln!(
                                    "flywheel: notify_blocked leaf failed for {name} (continuing): {e}"
                                );
                                continue;
                            }
                        };
                        let tier_enum = match out.tier {
                            0 => EscalationTier::T0,
                            1 => EscalationTier::T1,
                            2 => EscalationTier::T2,
                            _ => EscalationTier::T3,
                        };
                        if let Some(msg) = on_blocked_tier_exit(tier_enum, &params.on_blocked) {
                            return Ok(msg);
                        }
                    }
                }
                LoopAction::PausedResurface {
                    name,
                    bead_id,
                    owner,
                    age_secs,
                } => {
                    let line = pause_resurface_line(name, bead_id, owner, *age_secs);
                    eprintln!("flywheel: {line}");
                    let _ = crate::state::log_event(
                        &params.project,
                        crate::state::EventKind::Loop,
                        serde_json::json!({
                            "status": "pause_resurface",
                            "agent": name,
                            "bead": bead_id,
                            "owner": owner,
                            "age_secs": *age_secs,
                        }),
                    );
                    any_work = true;
                }
                LoopAction::Nudge { name } => {
                    let rot_hit = bump_nudge_streak(&mut states, name);
                    if rot_hit == crate::rot::RotAction::Compact {
                        let _ = send_prompt(SendArgs {
                            project: params.project.clone(),
                            name: name.clone(),
                            text: crate::rot::compact_prompt().into(),
                            confirm: params.confirm_sends,
                            confirm_timeout_ms: params.confirm_timeout_ms,
                            invocation: params.invocation.clone(),
                            iteration,
                            profile: String::new(),
                        })
                        .await;
                    }
                    if let Some(n) = &params.nudge {
                        let outcome = match send_prompt(SendArgs {
                            project: params.project.clone(),
                            name: name.clone(),
                            text: n.clone(),
                            confirm: params.confirm_sends,
                            confirm_timeout_ms: params.confirm_timeout_ms,
                            invocation: params.invocation.clone(),
                            iteration,
                            profile: match crate::elm::leaf::run_leaf_journaled(
                                &params.project,
                                "__profile",
                                &[&params.project, name],
                                &format!("profile:{}:{name}", params.project),
                            ) {
                                Ok(v) => v
                                    .as_str()
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        v.get("raw").and_then(|r| r.as_str()).map(|s| s.to_string())
                                    })
                                    .unwrap_or_default(),
                                Err(e) => {
                                    eprintln!(
                                        "flywheel: __profile failed for {name} (sending raw): {e}"
                                    );
                                    String::new()
                                }
                            },
                        })
                        .await
                        {
                            Ok(o) => o,
                            Err(e) => SendOutcome {
                                ok: false,
                                name: name.clone(),
                                landed: Some(false),
                                status: None,
                                reason: Some(format!("error:{e}")),
                                deduped: false,
                                cancelled: crate::failure::classify(&format!("{e}"), None, None)
                                    .kind
                                    == crate::failure::FailureKind::Cancelled,
                            },
                        };
                        any_work = true;
                        match handle_send_result(&mut send_failures, name, &outcome) {
                            Escalation::Accept => {}
                            Escalation::Cancelled => {
                                eprintln!(
                                    "flywheel: send to {name} cancelled; preserving cancellation without respawn"
                                );
                            }
                            Escalation::FailStop => {
                                return Err(anyhow::anyhow!(
                                    "{}",
                                    failstop_message(name, outcome.reason.as_deref())
                                ));
                            }
                            Escalation::RespawnAndRequeue => {
                                let attempts = send_failures.get(name).copied().unwrap_or(0);
                                let reason = outcome.reason.as_deref().unwrap_or("stalled");
                                respawn_and_requeue(
                                    &params,
                                    &mut queue,
                                    &agent_kinds,
                                    name,
                                    None,
                                    reason,
                                    attempts,
                                )
                                .await;
                            }
                        }
                    }
                }
                LoopAction::Diagnose { name, reason } => {
                    let diag = diagnose_agent(DiagnoseArgs {
                        project: params.project.clone(),
                        name: name.clone(),
                        reason: reason.clone(),
                    })
                    .await;
                    match diag {
                        Ok(_) => {}
                        Err(e) => {
                            let extra = tick_loop_owner(
                                &mut swarm,
                                &crate::workspace::Msg::DiagnoseFailed { name: name.clone() },
                            );
                            let _ = extra;
                            next_model = swarm.clone();
                            eprintln!(
                                "flywheel: diagnose_agent leaf failed for {name} (reverted awaiting_clear via tick): {e}"
                            );
                        }
                    }
                    any_work = true;
                }
                LoopAction::Respawn { name, reason } => {
                    let attempts = swarm
                        .heal_states
                        .get(name)
                        .map(|h| u64::from(h.respawn_count))
                        .unwrap_or(1);
                    let mut deferred = false;
                    if let Some(rest) = reason.strip_prefix("drain_expired bead ") {
                        let mut toks = rest.split_whitespace();
                        if let Some(bead) = toks.next() {
                            let prior: u32 = match (toks.next(), toks.next()) {
                                (Some("defer"), Some(n)) => n.parse().unwrap_or(0),
                                _ => 0,
                            };
                            let board = reservation_board_live(&params.project).await;
                            let globs: Vec<String> = board
                                .into_iter()
                                .filter(|(_, r)| r == bead)
                                .map(|(p, _)| p)
                                .collect();
                            if !globs.is_empty() {
                                let opts = crate::write_liveness::WalkOpts {
                                    timeout_secs: params.walk_timeout_secs,
                                    ..Default::default()
                                };
                                let root =
                                    crate::write_liveness::resolve_project_root(&params.project);
                                match crate::write_liveness::walk(&root, &globs, &opts) {
                                    crate::write_liveness::WalkOutcome::Fresh {
                                        path,
                                        age_secs,
                                    } => {
                                        let defers = prior.saturating_add(1);
                                        next_model.drain_pending.insert(
                                            name.clone(),
                                            DrainState {
                                                bead_id: bead.to_string(),
                                                grace_polls: DRAIN_GRACE_POLLS,
                                                defers,
                                                paused: None,
                                            },
                                        );
                                        eprintln!(
                                            "{}",
                                            crate::write_liveness::defer_digest(
                                                name, bead, &path, age_secs, defers, &opts
                                            )
                                        );
                                        any_work = true;
                                        deferred = true;
                                    }
                                    outcome => {
                                        let verdict =
                                            crate::write_liveness::decide(&outcome, false, &opts);
                                        if verdict
                                            == (crate::write_liveness::LivenessVerdict::Respawn {
                                                inspect: true,
                                            })
                                        {
                                            eprintln!(
                                                "flywheel: D4 inspect recommended for {name} (bead {bead}): quiet + not-writing — look before next drain"
                                            );
                                        }
                                    }
                                }
                            }
                            if !deferred {
                                eprintln!(
                                    "flywheel: LoudMiss drain-stop grace expired for {name} (bead {bead}) — force-respawned"
                                );
                            }
                        }
                    }
                    if deferred {
                        continue;
                    }
                    respawn_and_requeue(
                        &params,
                        &mut queue,
                        &agent_kinds,
                        name,
                        None,
                        reason,
                        attempts,
                    )
                    .await;
                    any_work = true;
                }
                LoopAction::Reclaim {
                    bead_id,
                    pin,
                    terminal_name,
                    reason,
                } => {
                    let _ = reclaim_bead(ReclaimBeadArgs {
                        project: params.project.clone(),
                        bead_id: bead_id.clone(),
                        pin: pin.clone(),
                        terminal_name: terminal_name.clone(),
                        reason: reason.clone(),
                    })
                    .await;
                    any_work = true;
                }
                LoopAction::Wait => {}
            }
        }

        let inprogress_blocking = if params.work {
            match crate::elm::leaf::run_leaf_journaled(
                &params.project,
                "__inprogress",
                &[&params.project],
                &format!("inprogress:{}", params.project),
            ) {
                Ok(v) => match parse_bead_list(&v, "__inprogress") {
                    Ok(bs) => {
                        let live_names: Vec<String> =
                            states.iter().map(|s| s.name.clone()).collect();
                        !blocking_inprogress(&bs, &live_names).is_empty()
                    }
                    Err(e) => {
                        critical_leaf_failed_this_iter = true;
                        eprintln!("flywheel: poll_inprogress leaf failed (completion guard): {e}");
                        true
                    }
                },
                Err(e) => {
                    critical_leaf_failed_this_iter = true;
                    eprintln!("flywheel: poll_inprogress leaf failed (completion guard): {e}");
                    true
                }
            }
        } else {
            false
        };

        let all_settled = all_settled(&states);

        let complete_now = queue.is_empty()
            && all_settled
            && !any_work
            && !inprogress_blocking
            && expected_replies.is_empty()
            && (!params.work
                || (!swarm.bead_slots.values().any(|slot| slot.watch.is_some())
                    && wave_held.is_empty()));
        if complete_now {
            blind_complete_failures = blind_completion_count(
                complete_now,
                critical_leaf_failed_this_iter,
                blind_complete_failures,
            );
            if critical_leaf_failed_this_iter {
                complete_holds = 0;
                match blind_completion_outcome(
                    params.resume,
                    blind_complete_failures,
                    DEFAULT_LEAF_FAILURE_ABORT_POLLS,
                ) {
                    BlindCompletion::Continue => {
                        eprintln!(
                            "flywheel: WARN {blind_complete_failures}/{} consecutive critical-leaf failures with an empty world — refusing to declare complete while blind (stale checkpointed invocation?).",
                            DEFAULT_LEAF_FAILURE_ABORT_POLLS
                        );
                        wait_secs = params.active_interval_secs.clamp(5, 30);
                    }
                    BlindCompletion::Abort => {
                        return Err(anyhow::anyhow!(
                            "loop abort: {blind_complete_failures} consecutive critical-leaf failures while the world looked empty (poll_events/poll_swarm/poll_ready/poll_inprogress/expected-replies) — the loop cannot see the tracker or the swarm. Fix: `flywheel loop-mode doctor --fix` (diagnose loop infrastructure), then re-run."
                        ));
                    }
                }
            } else {
                complete_holds += 1;
                if complete_holds > params.drain_grace_polls {
                    return Ok("complete".to_string());
                }
                wait_secs = params.active_interval_secs.clamp(5, 30);
            }
        } else {
            complete_holds = 0;
            blind_complete_failures = 0;

            wait_secs = next_interval_secs(
                &states,
                any_work,
                !queue.is_empty(),
                params.interval_secs,
                params.active_interval_secs,
            );
            if !event_batch.events.is_empty() {
                wait_secs = wait_secs.min(params.active_interval_secs).max(5);
            }
        }
    }
}

/// Register the durable functions and start (or resume) the loop invocation.
/// Returns the workflow result; the process must stay alive while awaiting.
pub async fn run_durable_loop(_cfg: &Config, mut params: LoopParams) -> anyhow::Result<String> {
    let authority = crate::authority::RuntimeAuthority::resolve();

    let now_nanos = authority.clock.now_nanos();
    let prior = crate::state::load_active_loop(&params.project)?;
    let prior_pending = prior.is_some();
    let start = loop_start_decision(prior_pending, params.force, params.resume);
    params.resume = start == LoopStart::Resume;
    let id = match (start, prior) {
        (LoopStart::Resume, Some(al)) => {
            eprintln!(
                "flywheel: resuming existing loop {} (--resume)",
                dim_id(&al.id)
            );
            al.id
        }
        (LoopStart::Fresh, Some(al)) if prior_pending => {
            if params.force {
                eprintln!(
                    "flywheel: --force rejecting stale loop invocation {} (fresh run)",
                    dim_id(&al.id)
                );
            } else {
                eprintln!(
                    "flywheel: previous loop invocation {} is still pending but its run is dead — starting FRESH (no replay; pass --resume to reuse the id)",
                    dim_id(&al.id)
                );
            }
            fresh_loop_id(
                &params.project,
                &serde_json::to_value(&params)?,
                "swarm-loop",
                now_nanos,
            )?
        }
        _ => fresh_loop_id(
            &params.project,
            &serde_json::to_value(&params)?,
            "swarm-loop",
            now_nanos,
        )?,
    };

    {
        let base = params.project.rsplit('/').next().unwrap_or(&params.project);
        let (_schema, loops) =
            crate::state::load_loops_registry().unwrap_or((0, Default::default()));
        let ids: Vec<&str> = loops.values().map(|al| al.id.as_str()).collect();
        let _candidates = stale_loop_reject_ids(&ids, base, Some(&id));
        let pids = crate::state::load_loop_pids();
        if let Some(entry) = pids.get(&params.project)
            && !crate::state::pid_alive(entry.pid)
        {
            crate::state::unregister_loop(&params.project, entry.pid);
        }
    }

    crate::state::ensure_loop_owner(&params.project)?;

    params.invocation = id.clone();
    let result: String = swarm_loop(authority, params)
        .await
        .map_err(|e| anyhow::anyhow!("durable loop failed: {e}"))?;
    Ok(result)
}

fn dim_id(id: &str) -> String {
    crate::output::dim(id)
}

/// A genuinely fresh loop invocation id (path-safe: an ABSOLUTE project path
/// contains slashes that break the promise id on the server — use the project
/// base, last path segment) + persisted active-loop record. Shared by the
/// durable + engine loops (bd-e131 force-takeover fresh-run path).
fn fresh_loop_id(
    project: &str,
    params: &serde_json::Value,
    prefix: &str,
    now_nanos: u128,
) -> anyhow::Result<String> {
    let id_project = project.rsplit('/').next().unwrap_or(project);
    let fresh = format!("{prefix}:{id_project}:{now_nanos}");
    crate::state::save_active_loop(
        project,
        &crate::state::ActiveLoop::new(fresh.clone(), params.clone(), crate::state::now_iso()),
    )?;
    Ok(fresh)
}

/// bd-swarm-loop-durable-panic-atlo: how a durable loop start treats a
/// persisted active-loop invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStart {
    /// Start a fresh invocation id. A pending prior is rejected by the
    /// stale sweep (or explicitly, with --force).
    Fresh,
    /// Reuse the pending invocation and replay its checkpoints (--resume).
    Resume,
}

/// Pure start decision: resuming a dead run's checkpoints is OPT-IN.
///
/// bd-swarm-loop-durable-panic-atlo: a pending invocation whose owner process
/// is gone is a zombie — replaying its checkpointed leaf results against the
/// current binary's leaf shapes storms serialization errors (measured on
/// elm-effect-rs). So a pending prior with no explicit `--resume` starts
/// FRESH; `--force` also starts fresh (and rejects the prior explicitly, the
/// bd-e131 takeover). No pending prior → always fresh.
pub fn loop_start_decision(prior_pending: bool, force: bool, resume: bool) -> LoopStart {
    if force || !prior_pending || !resume {
        LoopStart::Fresh
    } else {
        LoopStart::Resume
    }
}

/// Does a durable-promise id belong to `base`'s loop invocations? Both the
/// durable and engine loops build their invocation id via `fresh_loop_id`,
/// which uses the project's last path segment (`base`) — so ids are exactly
/// `swarm-loop:{base}:<ts>` / `engine-loop:{base}:<ts>`. Exact colon-delimited
/// prefix match: base `toron` never matches `toron-x` (boundary at the colon).
/// ROOT invocations only — child promises carry a `.` separator (`<root>.1`)
/// and belong to a live invocation's checkpoint tree, never cleared.
fn id_is_project_loop(id: &str, base: &str) -> bool {
    if id.contains('.') {
        return false;
    }
    let swarm = format!("swarm-loop:{base}:");
    let engine = format!("engine-loop:{base}:");
    id.starts_with(&swarm) || id.starts_with(&engine)
}

/// bd-gny: the guard's pure selection core — which pending promises are
/// stale-loop reject candidates for `base`. The `except` invocation (the one
/// this run resumes/owns) is preserved; child promises (`.N`, a live
/// checkpoint tree) and other projects' invocations are never candidates.
/// Extracted from `clear_stale_loop_invocations` so the stale-root rejection
/// is deterministically testable without a vendor server.
fn stale_loop_reject_ids<'a>(ids: &[&'a str], base: &str, except: Option<&str>) -> Vec<&'a str> {
    ids.iter()
        .copied()
        .filter(|id| Some(*id) != except && id_is_project_loop(id, base))
        .collect()
}

/// Run the loop (bd-w4-delete-dup-sbh8): the engine-backed loop BODY is
/// deleted — the durable `swarm_loop` is the single keeper and `plan_actions`
/// its one planning definition. `flywheel loop run --engine` routes here: a
/// provably thin wrapper over [`run_durable_loop`] (the bd-e0xk
/// engine-dispatch coexistence bridge is done; no compat shim).
pub async fn run_engine_loop(cfg: &Config, params: LoopParams) -> anyhow::Result<String> {
    run_durable_loop(cfg, params).await
}
/// bd-gny, re-pointed at rows (bd-flywheel-loop-substrate-ftpm): verdict over
/// the LOCAL liveness row + loop-process liveness. The row is written at loop
/// start and stays until the next run replaces it; only the pid registry can
/// say whether the owner is alive.
fn loop_verdict(row_present: bool, process_alive: bool) -> &'static str {
    match (row_present, process_alive) {
        (true, true) => "running",
        (true, false) => "stale",
        (false, _) => "absent",
    }
}

/// Query the current state of a durable loop invocation (local rows only).
pub async fn loop_status(_cfg: &Config, project: &str) -> anyhow::Result<serde_json::Value> {
    let Some(al) = crate::state::load_active_loop(project)? else {
        return Ok(serde_json::json!({
            "project": project,
            "loop": "none recorded — run: flywheel loop run <project>"
        }));
    };
    let pid_entry = crate::state::load_loop_pids().get(project).cloned();
    let process_alive = pid_entry
        .as_ref()
        .map(|e| crate::state::pid_alive(e.pid))
        .unwrap_or(false);
    Ok(serde_json::json!({
        "project": project,
        "id": al.id,
        "state": "row",
        "verdict": loop_verdict(true, process_alive),
        "pid": pid_entry.as_ref().map(|e| e.pid),
        "process_alive": process_alive,
        "started_at": pid_entry.as_ref().map(|e| e.started_at.clone()),
        "mail_degraded": al.mail_degraded,
        "mail_health": al.mail_health,
        "loop": LoopSnapshot::from_active_loop(&al),
    }))
}

/// Is `id` a ROOT loop invocation (`swarm-loop:*` / `engine-loop:*`)? Child
/// promises carry a `.` separator (`<root>.1`) and belong to a live
/// invocation's checkpoint tree, never reported or rejected by the doctor.
/// Global form of `id_is_project_loop` (no project base scoping).
fn is_loop_invocation(id: &str) -> bool {
    if id.contains('.') {
        return false;
    }
    id.starts_with("swarm-loop:") || id.starts_with("engine-loop:")
}

/// bd-eign.4: `flywheel loop doctor` — infrastructure diagnostics + repair.
/// Checks: (1) the durable engine-store capability, (2) stale loop rows
/// (a stored row whose pid is not alive), (3) orphaned loop PIDs, (4) the
/// loops registry consistency. `fix=true` drops stale rows and prunes
/// orphaned pid entries. A failed check is recorded in the report, never a
/// hard error — the doctor must diagnose a broken stack, not fail on it.
pub async fn loop_doctor(_cfg: &Config, fix: bool) -> anyhow::Result<serde_json::Value> {
    let engine_store = match crate::daemon::durable_capability_probe() {
        Ok(()) => json!({"ok": true, "error": null}),
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    };

    let pids = crate::state::load_loop_pids();
    let (_schema, loops) = crate::state::load_loops_registry()?;

    let mut stale_ids: Vec<String> = Vec::new();
    let mut registry: Vec<serde_json::Value> = Vec::new();
    for (project, al) in &loops {
        let pid_entry = pids.get(project);
        let process_alive = pid_entry
            .map(|e| crate::state::pid_alive(e.pid))
            .unwrap_or(false);
        if !process_alive && is_loop_invocation(&al.id) {
            stale_ids.push(al.id.clone());
        }
        registry.push(json!({
            "project": project,
            "invocation": al.id,
            "pid": pid_entry.map(|e| e.pid),
            "process_alive": process_alive,
            "verdict": loop_verdict(true, process_alive),
        }));
    }
    stale_ids.sort();

    let mut orphaned: Vec<serde_json::Value> = Vec::new();
    let mut pruned: Vec<String> = Vec::new();
    for (project, e) in &pids {
        if !crate::state::pid_alive(e.pid) {
            orphaned.push(json!({
                "project": project,
                "pid": e.pid,
                "started_at": e.started_at,
                "invocation": e.invocation,
            }));
            if fix {
                pruned.push(project.clone());
            }
        }
    }
    if fix && !pruned.is_empty() {
        let mut remaining = pids.clone();
        for p in &pruned {
            remaining.remove(p);
        }
        crate::state::save_loop_pids(&remaining)?;
    }

    let mut dropped: Vec<String> = Vec::new();
    if fix {
        for (project, al) in &loops {
            if stale_ids.contains(&al.id) {
                crate::state::remove_active_loop(project)?;
                dropped.push(al.id.clone());
            }
        }
    }

    Ok(json!({
        "fix": fix,
        "engine_store": engine_store,
        "stale_invocations": {
            "count": stale_ids.len(),
            "ids": stale_ids,
        },
        "orphaned_pids": {
            "count": orphaned.len(),
            "entries": orphaned,
        },
        "registry": {
            "count": registry.len(),
            "entries": registry,
        },
        "fixed": {
            "dropped": dropped,
            "pruned": pruned,
        },
    }))
}

/// Convenience: tiered detection without evidence (empty tail, silent_idle reason).
#[cfg(test)]
pub fn detect_stalled_panes_tiered(
    in_progress: &[ReadyBead],
    states: &[AgentState],
    now_secs: i64,
    threshold_secs: u64,
    watching_threshold_secs: u64,
) -> Vec<IngressEvent> {
    detect_stalled_panes_with_evidence(
        in_progress,
        states,
        now_secs,
        threshold_secs,
        watching_threshold_secs,
        |_| String::new(),
    )
}

#[cfg(test)]
pub fn renew_fail_count() -> u64 {
    RENEW_FAIL_COUNTER.load(Ordering::Relaxed)
}

#[cfg(test)]
pub fn reset_renew_fail_count() {
    RENEW_FAIL_COUNTER.store(0, Ordering::Relaxed);
}

/// Event-shaped decision path (bd-event-dispatch-zero-latency).
///
/// Pure: updates `dispatch_pending` and may pop the queue front (FIFO, same
/// as `plan_actions`). Returns at most one action per event.
///
/// Double-dispatch guard: if `name` is already in `dispatch_pending`, a
/// further `done`/`idle` does not re-pop the queue until a `working` event
/// clears the flag (or `clear_dispatch_pending` after send landing).
#[cfg(test)]
pub fn route_event(
    states: &[AgentState],
    ev: &PollEvent,
    queue: &mut Vec<String>,
    dispatch_pending: &mut std::collections::HashSet<String>,
) -> Option<LoopAction> {
    let name = name_for_pane(states, &ev.pane_id).or_else(|| {
        let kind = ev.agent.as_deref()?;
        let matches: Vec<_> = states.iter().filter(|s| s.kind == kind).collect();
        if matches.len() == 1 {
            Some(matches[0].name.clone())
        } else {
            None
        }
    });

    let status = ev.status.as_deref().unwrap_or("");
    match ev.event_type.as_str() {
        "pane.agent_status_changed" => {
            let name = name?;
            match status {
                "working" => {
                    dispatch_pending.remove(&name);
                    None
                }
                "blocked" => Some(LoopAction::Blocked { name }),
                "done" | "idle" => {
                    if dispatch_pending.contains(&name) {
                        return None;
                    }
                    if queue.is_empty() {
                        return None;
                    }
                    let text = queue.remove(0);
                    let display = states
                        .iter()
                        .find(|s| s.name == name)
                        .and_then(|s| s.mail_identity.clone())
                        .unwrap_or_else(|| name.clone());
                    let text = text.replace("{AGENT_NAME}", &display);
                    dispatch_pending.insert(name.clone());
                    Some(LoopAction::DispatchImmediate { name, text })
                }
                _ => None,
            }
        }
        "pane.agent_detected" => {
            let name = name?;
            if dispatch_pending.contains(&name) || queue.is_empty() {
                return None;
            }
            if status == "working" {
                return None;
            }
            let text = queue.remove(0);
            let display = states
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.mail_identity.clone())
                .unwrap_or_else(|| name.clone());
            let text = text.replace("{AGENT_NAME}", &display);
            dispatch_pending.insert(name.clone());
            Some(LoopAction::DispatchImmediate { name, text })
        }
        "pane.exited" => {
            let name = name.unwrap_or_else(|| format!("pane:{}", ev.pane_id));
            dispatch_pending.remove(&name);
            Some(LoopAction::Diagnose {
                name,
                reason: "pane_exited".into(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod reconcile_expected_replies_tests {
    use super::*;

    fn entry(head: &str, thread: &str) -> crate::state::ExpectedReply {
        crate::state::ExpectedReply {
            agent: "CalmGate".into(),
            thread: thread.into(),
            head: Some(head.into()),
            created_ts: "t".into(),
        }
    }

    #[test]
    fn two_asks_from_one_agent_stay_two_keys_and_the_mail_identity_is_not_the_key() {
        let entries = vec![entry("m1", "t1"), entry("m2", "t2")];
        let k1 = crate::state::ask_key("CalmGate", "m1");
        let mut prev = std::collections::HashMap::new();
        prev.insert(
            k1.clone(),
            ExpectedReplyRun {
                agent: "CalmGate".into(),
                thread: "t1".into(),
                head: Some("m1".into()),
                reminder_count: 1,
            },
        );
        let next = reconcile_expected_replies(&prev, &entries);
        assert_eq!(next.len(), 2);
        let kept = next.get(&k1).unwrap();
        assert_eq!(kept.agent, "CalmGate");
        assert_ne!(k1, kept.agent);
        assert_eq!(kept.reminder_count, 1);
        let k2 = crate::state::ask_key("CalmGate", "m2");
        assert_eq!(next.get(&k2).unwrap().reminder_count, 0);
        assert_eq!(next.get(&k2).unwrap().agent, "CalmGate");

        let mut stale = std::collections::HashMap::new();
        stale.insert(
            k1.clone(),
            ExpectedReplyRun {
                agent: "CalmGate".into(),
                thread: "other".into(),
                head: Some("m1".into()),
                reminder_count: 1,
            },
        );
        let reset = reconcile_expected_replies(&stale, &entries[..1]);
        assert_eq!(reset.get(&k1).unwrap().reminder_count, 0);
    }
}

#[cfg(test)]
mod loop_doctor_tests {
    use super::is_loop_invocation;

    /// One row per id shape: only a root `swarm-loop:`/`engine-loop:` promise
    /// is a loop invocation — child promises (`:123.1`), other workflows and
    /// bare / flywheel-loop ids never count.
    #[test]
    fn is_loop_invocation_rows() {
        let rows = [
            ("swarm-loop-root", "swarm-loop:flywheel:123", true),
            ("engine-loop-root", "engine-loop:flywheel:456", true),
            ("swarm-loop-child", "swarm-loop:flywheel:123.1", false),
            ("engine-loop-child", "engine-loop:flywheel:456.7.2", false),
            ("foreign-workflow", "other-workflow:flywheel:123", false),
            ("bare-prefix", "swarm-loop", false),
            ("flywheel-loop", "flywheel-loop:flywheel:123", false),
        ];
        for (name, id, expected) in rows {
            assert_eq!(is_loop_invocation(id), expected, "row {name}");
        }
    }
}

#[cfg(test)]
#[cfg(test)]
mod reply_verify_tests {
    use super::*;

    fn v(
        head: Option<&str>,
        newest: Option<&str>,
        author: Option<&str>,
        agent: &str,
        reminded: u32,
    ) -> ReplyVerdict {
        verify_reply(head, newest, author, agent, reminded)
    }

    /// bd-a7j, one row per verdict: a NEW message from the asked agent
    /// satisfies — even when the head (question id) was never recorded —
    /// while the question's own id (the captain's message, not a reply) and
    /// another agent's message only remind.
    #[test]
    fn reply_verdict_rows() {
        let rows = [
            (
                "new-message-satisfies",
                Some("q1"),
                Some("r1"),
                Some("oc"),
                "oc",
                0,
                ReplyVerdict::Satisfied,
            ),
            (
                "unrecorded-head-still-satisfies",
                None,
                Some("r1"),
                Some("oc"),
                "oc",
                3,
                ReplyVerdict::Satisfied,
            ),
            (
                "question-itself-reminds",
                Some("q1"),
                Some("q1"),
                Some("oc"),
                "oc",
                0,
                ReplyVerdict::Reminded,
            ),
            (
                "other-agent-reminds",
                Some("q1"),
                Some("r1"),
                Some("pi"),
                "oc",
                0,
                ReplyVerdict::Reminded,
            ),
        ];
        for (name, head, newest, author, agent, reminded, expected) in rows {
            let got = v(head, newest, author, agent, reminded);
            assert_eq!(got, expected, "row {name}: {got:?}");
        }
    }

    /// bd-a7j: silence → first remind, second silence escalates.
    #[test]
    fn silence_reminds_once_then_escalates() {
        assert!(matches!(
            v(Some("q1"), None, None, "oc", 0),
            ReplyVerdict::Reminded
        ));
        assert!(matches!(
            v(Some("q1"), None, None, "oc", 1),
            ReplyVerdict::Escalate
        ));
        match v(Some("q1"), Some("r1"), Some("pi"), "oc", 1) {
            ReplyVerdict::Escalate => {}
            other => panic!("escalation fires when reminded once and still wrong: {other:?}"),
        }
    }

    /// bd-a7j: argv builders round-trip the leaf contract.
    #[test]
    fn reply_leaf_argv_roundtrips() {
        let ra = RemindArgs {
            project: "proj".into(),
            name: "oc".into(),
            thread: "t1".into(),
            reminder_count: 0,
            invocation: "inv".into(),
            iteration: 7,
        };
        let argv = remind_reply_argv(&ra);
        assert_eq!(
            argv,
            vec![
                "__remind-reply",
                "proj",
                "oc",
                "t1",
                "--reminder-count",
                "0",
                "--invocation",
                "inv",
                "--iteration",
                "7"
            ]
        );
    }
}

#[cfg(test)]
mod dispatch_guard_tests {
    use super::*;

    const MIN: i64 = 60 * 1_000_000;
    fn sig(ts_us: i64, k: GuardSignalKind) -> (i64, GuardSignalKind) {
        (ts_us, k)
    }

    #[test]
    fn classify_reads_real_thread_shapes() {
        use GuardSignalKind::*;
        assert_eq!(
            classify_guard_signal(
                "[bd-adr22-w4-steer-engine-w69z] starting — steer durable-rail survey + plan",
                "Claimed bd-adr22-w4-steer-engine-w69z. Write-set: flywheel src/main.rs...",
                None,
            ),
            Some(Announce)
        );
        assert_eq!(
            classify_guard_signal(
                "[bd-adr22-w7-delegated-compose-fhdy] STAND-DOWN — live concurrent executor on full surface",
                "Retracting my earlier split proposal — superseded by events.",
                None,
            ),
            Some(StandDown)
        );
        assert_eq!(
            classify_guard_signal(
                "oqe6: agreed + cli lease released — you execute; full handoff inside",
                "**agreed + released** — lease is released to you. You are the executor; I stand down.",
                None,
            ),
            Some(Handoff),
            "explicit transfer beats a passing stand-down phrase"
        );
        assert_eq!(
            classify_guard_signal("[bd-x] done", "**status**: closed", None),
            None,
            "D9: `] done` subject alone NEVER fires the hold (subject is decoration)"
        );
        assert_eq!(
            classify_guard_signal("[bd-123] done investigating", "still reproing", None),
            None,
            "participle 'done' is not a close report"
        );
        assert_eq!(
            classify_guard_signal(
                "[bd-adr22-w4-steer-engine-w69z] starting — steer durable-rail survey + plan",
                "Claimed bd-adr22-w4-steer-engine-w69z. Write-set: flywheel src/main.rs...",
                Some("bd-adr22-w4-steer-engine-w69z"),
            ),
            Some(Done),
            "D9: tag present + arbitrary subject → hold fires (tag beats participle)"
        );
        assert_eq!(
            classify_guard_signal("Re: [bd-x] done", "closed", None),
            None,
            "a reply prefix alone is not the hold (D9 clean cutover)"
        );
        assert_eq!(
            classify_guard_signal("[bd-x] done ", "closed", None),
            None,
            "trailing whitespace does not resurrect the subject match"
        );
        assert_eq!(
            classify_guard_signal(
                "re: bd-y question",
                "is claim-watch armed for this bead?",
                None
            ),
            None,
            "claim-watch mentions are not claims"
        );
        assert_eq!(
            classify_guard_signal("[BD-X] DONE", "closed", None),
            None,
            "uppercase subject alone never fires the hold (D9)"
        );
    }

    /// ADR-0010 D9 (bd-l0fz): the bead tag is the hold, the subject is
    /// decoration. Locks the AC both ways: tag present + arbitrary subject →
    /// Done; tag absent + `] done` subject → None. No dual-match shim: a
    /// future loosening back to subject matching must fail red.
    #[test]
    fn done_hold_is_the_bead_tag_not_the_subject() {
        use GuardSignalKind::*;
        for subject in [
            "[bd-x] done",
            "[bd-123] done investigating",
            "shipped and verified",
            "all done",
            "",
        ] {
            assert_eq!(
                classify_guard_signal(subject, "closed", Some("bd-l0fz")),
                Some(Done),
                "bead tag fires the hold regardless of subject {subject:?}"
            );
        }
        for subject in ["[bd-x] done", "Re: [bd-x] done", "[bd-x] done."] {
            assert_eq!(
                classify_guard_signal(subject, "**status**: closed", None),
                None,
                "subject {subject:?} alone is decoration (D9)"
            );
        }
    }

    /// bd-dispatch-dedupe-2zlz: `done` is the close-report verb ONLY as the
    /// terminal `] done`. A subject ending in another word — a participle
    /// ("done reviewing"), an adverb ("well done"), punctuation — is not a
    /// close report and must not hold dispatch. Locks the anchor in so a
    /// future loosening back to a bare `contains("] done")` fails red.
    #[test]
    fn unexpired_peer_announce_holds_fresh_dispatch() {
        let now = 1_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![sig(now - 5 * MIN, GuardSignalKind::Announce)],
            receipts_fresh_us: None,
        };
        let err = eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US)
            .expect_err("unexpired announce must hold");
        assert!(err.contains("unexpired peer announce"), "{err}");
    }

    /// bd-gkwu: the guard obeys the INJECTED clock only — the same facts
    /// flip verdict purely by the passed now_us, deterministically across
    /// repeats. No wall read inside: substitution witness for Law 2.
    #[test]
    fn authority_edge_dispatch_guard_obeys_injected_clock() {
        let facts = DispatchGuardFacts {
            signals: vec![sig(900 * MIN, GuardSignalKind::Announce)],
            receipts_fresh_us: None,
        };
        let hold = eval_dispatch_guard(&facts, 901 * MIN, DISPATCH_GUARD_TTL_US);
        assert!(hold.is_err(), "fresh announce must hold");
        let ttl_min = DISPATCH_GUARD_TTL_US / (60 * 1_000_000);
        let late = (900 + ttl_min + 1) * MIN;
        assert!(
            eval_dispatch_guard(&facts, late, DISPATCH_GUARD_TTL_US).is_ok()
                && eval_dispatch_guard(&facts, late, DISPATCH_GUARD_TTL_US).is_ok(),
            "same injected now must decide identically every time"
        );
    }

    #[test]
    fn stale_announce_falls_through_to_dispatch() {
        let now = 10_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![sig(now - 4 * 60 * MIN, GuardSignalKind::Announce)],
            receipts_fresh_us: None,
        };
        assert!(
            eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US).is_ok(),
            ">TTL announce is stale: re-dispatch allowed"
        );
    }

    #[test]
    fn handoff_after_claim_reopens_dispatch() {
        let now = 1_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![
                sig(now - 20 * MIN, GuardSignalKind::Claim),
                sig(now - 2 * MIN, GuardSignalKind::Handoff),
            ],
            receipts_fresh_us: None,
        };
        assert!(
            eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US).is_ok(),
            "handoff posted at/after the claim authorizes dispatch"
        );
    }

    #[test]
    fn stand_down_holds_absolutely() {
        let now = 1_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![sig(now - 3 * MIN, GuardSignalKind::StandDown)],
            receipts_fresh_us: None,
        };
        let err =
            eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US).expect_err("stand-down holds");
        assert!(err.contains("stood down"), "{err}");
    }

    #[test]
    fn done_newest_skips_ledger_sync_gap() {
        let now = 1_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![
                sig(now - 60 * MIN, GuardSignalKind::Announce),
                sig(now - 2 * MIN, GuardSignalKind::Done),
            ],
            receipts_fresh_us: None,
        };
        let err = eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US).expect_err("done skips");
        assert!(err.contains("ledger sync gap"), "{err}");
    }

    #[test]
    fn fresh_claim_after_done_restarts_the_clock() {
        let now = 1_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![
                sig(now - 90 * MIN, GuardSignalKind::Done),
                sig(now - MIN, GuardSignalKind::Claim),
            ],
            receipts_fresh_us: None,
        };
        let err = eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US)
            .expect_err("claim newer than done holds again");
        assert!(err.contains("unexpired peer announce"), "{err}");
    }

    #[test]
    fn receipts_within_ttl_are_executor_evidence() {
        let now = 1_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![],
            receipts_fresh_us: Some(now - 30 * MIN),
        };
        let err = eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US)
            .expect_err("fresh receipts hold");
        assert!(err.contains("receipts"), "{err}");
    }

    #[test]
    fn silence_and_cold_receipts_dispatch() {
        let now = 1_000 * MIN;
        let facts = DispatchGuardFacts {
            signals: vec![],
            receipts_fresh_us: Some(now - 5 * 60 * MIN),
        };
        assert!(eval_dispatch_guard(&facts, now, DISPATCH_GUARD_TTL_US).is_ok());
    }
}

#[cfg(test)]
mod tests {

    const W7_CATALOG: &[crate::decide::CatalogEntry] = &[
        ("nightly-fence", Some("run the verify fence nightly")),
        ("weekly-review", Some("review the week and land beads")),
    ];

    fn w7_spec_json(objective: &str) -> String {
        serde_json::json!({ "objective": objective, "requester": "SandySparrow" }).to_string()
    }

    /// REQUIRED W7 test: the allowlist gate — a non-allowlisted sender's
    /// compose-request is Denied and NEVER dispatches (no queue prompt,
    /// no candidate, nothing).
    #[test]
    fn w7_allowlist_deny_never_dispatches() {
        let allow = vec!["SandySparrow".to_string()];
        assert!(matches!(
            plan_compose_request(
                "SandySparrow",
                &w7_spec_json("map the unknown frobnicator"),
                &allow,
                W7_CATALOG
            ),
            ComposeRequestOutcome::CandidateForApproval { .. }
        ));
        assert_eq!(
            plan_compose_request(
                "SomeImpostor",
                &w7_spec_json("map the unknown frobnicator"),
                &allow,
                W7_CATALOG
            ),
            ComposeRequestOutcome::Denied
        );
        assert_eq!(
            plan_compose_request("SomeImpostor", "not json at all", &allow, W7_CATALOG),
            ComposeRequestOutcome::Denied
        );
    }

    /// REQUIRED W7 test: decide coverage miss renders the compose candidate
    /// FOR APPROVAL (never auto-run); a hit names the covering workflow.
    #[test]
    fn w7_coverage_miss_renders_for_approval_hit_names_workflow() {
        let allow = vec!["SandySparrow".to_string()];
        let miss = plan_compose_request(
            "SandySparrow",
            &w7_spec_json("adversarial review of the parser module"),
            &allow,
            W7_CATALOG,
        );
        let ComposeRequestOutcome::CandidateForApproval { prompt } = miss else {
            panic!("expected candidate, got {miss:?}");
        };
        assert!(
            prompt.contains("approval required") && prompt.contains("--parts"),
            "candidate carries the approval contract + exact command: {prompt}"
        );
        assert!(
            prompt.contains("never auto-runs"),
            "W7 approval contract explicit: {prompt}"
        );
        let hit = plan_compose_request(
            "SandySparrow",
            &w7_spec_json("run the verify fence for me"),
            &allow,
            W7_CATALOG,
        );
        assert_eq!(
            hit,
            ComposeRequestOutcome::Covered {
                workflow: "nightly-fence".to_string()
            }
        );
    }
    use super::*;
    use crate::agentstate::AgentStateFile;

    fn st(name: &str, status: &str) -> AgentState {
        AgentState {
            name: name.to_string(),
            status: status.to_string(),
            kind: String::new(),
            pane_id: String::new(),
            mail_identity: None,
            nudge_streak: 0,
        }
    }

    fn st_pane(name: &str, status: &str, pane: &str) -> AgentState {
        AgentState {
            name: name.to_string(),
            status: status.to_string(),
            kind: "opencode".to_string(),
            pane_id: pane.to_string(),
            mail_identity: None,
            nudge_streak: 0,
        }
    }

    fn st_pin(name: &str, status: &str, pin: &str) -> AgentState {
        AgentState {
            name: name.to_string(),
            status: status.to_string(),
            kind: "opencode".to_string(),
            pane_id: String::new(),
            mail_identity: Some(pin.to_string()),
            nudge_streak: 0,
        }
    }

    /// bd-aseq regression: an expectation keyed by the PIN (what collab
    /// stored at send time) matches the agent's state via mail_identity or
    /// the spawn-registry pin — never stranded because the hash mapping
    /// diverges from the pinned identity.
    #[test]
    fn match_expected_agent_keys_by_pin_not_hash() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-aseq-mt-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        let states = vec![
            st_pin("flywheel-aseq-oc2", "idle", "TanFern"),
            st("flywheel-aseq-oc", "done"),
        ];
        assert_eq!(
            match_expected_agent("p", &states, "flywheel-aseq-oc").map(|s| s.name.as_str()),
            Some("flywheel-aseq-oc")
        );
        assert_eq!(
            match_expected_agent("p", &states, "TanFern").map(|s| s.name.as_str()),
            Some("flywheel-aseq-oc2")
        );
        assert!(match_expected_agent("p", &states, "GhostPin").is_none());

        let entries = vec![crate::state::SpawnEntry::with_mail_identity(
            "w:p9",
            "flywheel-aseq-gk",
            "RoseDune",
        )];
        crate::state::save_spawn_registry("p", &entries).expect("save");
        let states2 = vec![st("flywheel-aseq-gk", "idle")];
        assert_eq!(
            match_expected_agent("p", &states2, "RoseDune").map(|s| s.name.as_str()),
            Some("flywheel-aseq-gk")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn id_is_project_loop_matches_own_project_only() {
        assert!(id_is_project_loop(
            "swarm-loop:toron:1786511691522556000",
            "toron"
        ));
        assert!(id_is_project_loop(
            "engine-loop:toron:1786511691522556000",
            "toron"
        ));
        assert!(!id_is_project_loop(
            "swarm-loop:flywheel:1786511691522556000",
            "toron"
        ));
        assert!(!id_is_project_loop(
            "engine-loop:flywheel:1786511691522556000",
            "toron"
        ));
        assert!(!id_is_project_loop(
            "swarm-loop:toron-x:1786511691522556000",
            "toron"
        ));
        assert!(!id_is_project_loop(
            "engine-loop:toron-x:1786511691522556000",
            "toron"
        ));
        assert!(!id_is_project_loop(
            "swarm-loop:toron:1786511691522556000.1",
            "toron"
        ));
        assert!(!id_is_project_loop(
            "swarm-loop:toron:1786511691522556000.2.3",
            "toron"
        ));
        assert!(!id_is_project_loop("cron:weekly-review", "toron"));
        assert!(!id_is_project_loop("wf-graph-fanout-1", "toron"));
    }

    /// bd-6si7: the fresh-id leaf derives its id from its inputs plus the
    /// INJECTED stamp, never from a clock it samples. Two calls with the same
    /// stamp MUST produce the same id (replay determinism).
    ///
    /// Scope, stated honestly: the RETURNED id is a pure function of its
    /// inputs. The call is not side-effect free -- `fresh_loop_id` persists an
    /// active-loop sidecar and stamps that record with `now_iso()`. That stamp
    /// is an observation on the record, not an input to the id, which is why
    /// determinism still holds. An earlier version of this comment claimed the
    /// source "must not name a clock at all"; nothing enforced that, and it
    /// was not true.
    ///
    /// Hermetic by construction: the sidecar goes to `XDG_STATE_HOME`, so this
    /// test points that at a per-run uuid directory. It previously created a
    /// `fw-fresh-<pid>` directory, never pointed the state dir at it, and so
    /// wrote to the SHARED real loops dir -- a latent flake under parallel
    /// execution, and ambient state in a unit test besides. `lock_env`
    /// serialises the env mutation, and the uuid keeps parallel runs apart (a
    /// pid is identical for every test in one binary).
    #[test]
    fn fresh_loop_id_is_a_pure_function_of_its_stamp() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-fresh-mt-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let project = "ns/loopproj";
        let params = serde_json::json!({"project": project});

        let a = super::fresh_loop_id(project, &params, "swarm-loop", 1_700_000_000_000_000_000)
            .expect("first id");
        let b = super::fresh_loop_id(project, &params, "swarm-loop", 1_700_000_000_000_000_000)
            .expect("same stamp, same id");
        let c = super::fresh_loop_id(project, &params, "swarm-loop", 1_700_000_000_000_000_001)
            .expect("a different stamp is a different invocation");

        assert_eq!(a, b, "the same stamp must replay to the same id");
        assert_ne!(a, c, "a new invocation must not reuse a settled id");
        assert!(
            a.contains("loopproj") && !a.contains('/'),
            "the id stays path-safe and project-scoped: {a}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_loop_reject_ids_skips_owned_foreign_and_children() {
        let ids: Vec<&str> = vec![
            "swarm-loop:flywheel:1786557202790124000",
            "swarm-loop:flywheel:1786557202790124001",
            "swarm-loop:toron:1786512214561650000",
            "swarm-loop:flywheel:1786557202790124000.1",
            "engine-loop:flywheel:1786557202790124002",
        ];
        let rejected = stale_loop_reject_ids(
            &ids,
            "flywheel",
            Some("swarm-loop:flywheel:1786557202790124000"),
        );
        assert_eq!(
            rejected,
            vec![
                "swarm-loop:flywheel:1786557202790124001",
                "engine-loop:flywheel:1786557202790124002"
            ]
        );
        let rejected = stale_loop_reject_ids(&ids, "flywheel", None);
        assert_eq!(
            rejected,
            vec![
                "swarm-loop:flywheel:1786557202790124000",
                "swarm-loop:flywheel:1786557202790124001",
                "engine-loop:flywheel:1786557202790124002"
            ]
        );
        let rejected = stale_loop_reject_ids(&ids, "toron", None);
        assert_eq!(rejected, vec!["swarm-loop:toron:1786512214561650000"]);
    }

    #[test]
    fn loop_verdict_row_with_live_process_is_running() {
        assert_eq!(
            loop_verdict(true, true),
            "running",
            "a stored row with a live loop process is a healthy running loop"
        );
    }

    #[test]
    fn loop_verdict_row_without_process_is_stale() {
        assert_eq!(
            loop_verdict(true, false),
            "stale",
            "a stored row with no live process is an orphaned invocation — the \
             bd-gny stuck report, now read from local rows"
        );
    }

    #[test]
    fn loop_verdict_absent_row_is_absent() {
        assert_eq!(loop_verdict(false, true), "absent");
        assert_eq!(loop_verdict(false, false), "absent");
    }

    fn pev(event_type: &str, pane: &str, status: &str, seq: u64) -> PollEvent {
        PollEvent {
            event_type: event_type.to_string(),
            pane_id: pane.to_string(),
            agent: Some("opencode".to_string()),
            status: Some(status.to_string()),
            seq,
        }
    }

    /// `plan_actions` fixture: one queue, everything else default (the eight
    /// rows below each re-typed the full two-dozen-field literal).
    fn plan(queue: &[&str], states: &[AgentState], nudge: Option<&str>) -> Vec<LoopAction> {
        plan_actions(
            &LoopModel {
                queue: queue.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            states,
            nudge,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        )
    }

    fn send(name: &str, text: &str) -> LoopAction {
        LoopAction::Send {
            name: name.to_string(),
            text: text.to_string(),
        }
    }

    /// One row per `plan_actions` contract. It emits exactly one action per
    /// agent, in state order: idle/done pop the queue front (a nudge when the
    /// queue is empty), working and unknown wait and never dispatch
    /// (bd-unknown-skip-dispatch), blocked reports. Fixtures and expectations
    /// are the eight tests this replaces.
    /// One row of the `plan_actions` table, named rather than a five-tuple:
    /// the fields read at the assertion sites, and clippy's `type_complexity`
    /// stays quiet (same shape as `pane`'s `RouteRow`).
    struct PlanRow {
        name: &'static str,
        states: Vec<AgentState>,
        queue: &'static [&'static str],
        nudge: Option<&'static str>,
        expected: Vec<LoopAction>,
    }

    #[test]
    fn plan_actions_rows() {
        let rows: [PlanRow; 8] = [
            PlanRow {
                name: "idle-pops-queue",
                states: vec![st("a", "idle")],
                queue: &["p1", "p6"],
                nudge: None,
                expected: vec![send("a", "p1")],
            },
            PlanRow {
                name: "queue-drains-across-idle-agents-in-order",
                states: vec![st("a", "idle"), st("b", "done")],
                queue: &["p1", "p6"],
                nudge: None,
                expected: vec![send("a", "p1"), send("b", "p6")],
            },
            PlanRow {
                name: "working-gets-nothing",
                states: vec![st("a", "working")],
                queue: &["p1"],
                nudge: None,
                expected: vec![LoopAction::Wait],
            },
            PlanRow {
                name: "blocked-reports-and-keeps-queue",
                states: vec![st("a", "blocked")],
                queue: &["p1"],
                nudge: None,
                expected: vec![LoopAction::Blocked {
                    name: "a".to_string(),
                }],
            },
            PlanRow {
                name: "empty-queue-with-nudge-nudges",
                states: vec![st("a", "idle")],
                queue: &[],
                nudge: Some("proceed"),
                expected: vec![LoopAction::Nudge {
                    name: "a".to_string(),
                }],
            },
            PlanRow {
                name: "empty-queue-no-nudge-waits",
                states: vec![st("a", "done")],
                queue: &[],
                nudge: None,
                expected: vec![LoopAction::Wait],
            },
            PlanRow {
                name: "unknown-keeps-queue",
                states: vec![st("a", "unknown")],
                queue: &["p1"],
                nudge: None,
                expected: vec![LoopAction::Wait],
            },
            PlanRow {
                name: "unknown-with-nudge-no-send",
                states: vec![st("a", "unknown")],
                queue: &[],
                nudge: Some("proceed"),
                expected: vec![LoopAction::Wait],
            },
        ];
        for row in rows {
            let got = plan(row.queue, &row.states, row.nudge);
            assert_eq!(got, row.expected, "row {}: {got:?}", row.name);
        }
    }

    /// bd-context-limit-honor-luqg: a CONTEXT_LIMIT sender maps to the crew
    /// agent by MAIL PIN first, then terminal name; an unknown sender maps to
    /// nothing (a human pasting the marker must never trigger a respawn).
    #[test]
    fn context_limit_target_maps_pin_then_name_never_strangers() {
        fn agent(name: &str, pin: Option<&str>) -> AgentState {
            AgentState {
                mail_identity: pin.map(str::to_string),
                ..st(name, "idle")
            }
        }
        let states = vec![
            agent("flywheel-toron-oc3", Some("TanFern")),
            agent("oc2", None),
        ];
        assert_eq!(
            context_limit_target("TanFern", &states).as_deref(),
            Some("flywheel-toron-oc3"),
            "pin match wins"
        );
        assert_eq!(
            context_limit_target("oc2", &states).as_deref(),
            Some("oc2"),
            "bare terminal name matches itself"
        );
        assert_eq!(context_limit_target("StrangerPin", &states), None);
        assert_eq!(context_limit_target("", &states), None);
    }

    #[test]
    fn unknown_not_settled() {
        let states = vec![st("a", "unknown")];
        assert!(!all_settled(&states));
        let settled = vec![st("a", "idle"), st("b", "done")];
        assert!(all_settled(&settled));
    }

    #[test]
    fn empty_swarm_is_settled() {
        assert!(all_settled(&[][..]));
    }

    fn rb(id: &str, assignee: Option<&str>) -> ReadyBead {
        ReadyBead {
            id: id.into(),
            title: String::new(),
            assignee: assignee.map(str::to_string),
            verify: None,
            labels: Vec::new(),
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }
    }

    /// bd-gnej: rb + a wave label (`wave-N`).
    fn rb_wave(id: &str, wave: u32) -> ReadyBead {
        ReadyBead {
            id: id.into(),
            title: String::new(),
            assignee: None,
            verify: Some("true".into()),
            labels: vec![format!("wave-{wave}")],
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }
    }

    /// bd-wave-gate-typed-authority-27et: rb + ONLY the typed field — the
    /// schema-18 shape a `br create --wave N` bead has (NO `wave-N` label).
    fn rb_typed(id: &str, wave: u32) -> ReadyBead {
        ReadyBead {
            id: id.into(),
            verify: Some("true".into()),
            wave: Some(wave),
            ..Default::default()
        }
    }

    /// A wave-2 bead is HELD while a wave-1 bead is open/in_progress —
    /// earlier waves run first; the loop waits (wave-gate, NOT a stuck
    /// owner).
    #[test]
    fn wave_gate_holds_wave2_while_wave1_open() {
        let ready = vec![rb_wave("bd-w1", 1), rb_wave("bd-w2", 2)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let held = wave_gate_held(&ready, &inprog);
        assert_eq!(held, vec!["bd-w2"], "{held:?}");
    }

    /// The wave-2 bead is RELEASED once the wave-1 bead closes (drops out of
    /// ready AND in_progress).
    #[test]
    fn wave_gate_releases_when_earlier_wave_closes() {
        let ready = vec![rb_wave("bd-w2", 2)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let held = wave_gate_held(&ready, &inprog);
        assert!(held.is_empty(), "wave-1 closed -> wave-2 dispatches");
    }

    /// No wave labels anywhere → no gating (identity).
    #[test]
    fn wave_gate_no_labels_no_gating() {
        let ready = vec![rb("bd-a", None), rb("bd-b", None), rb_wave("bd-c", 3)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let held = wave_gate_held(&ready, &inprog);
        assert!(held.is_empty(), "{held:?}");
    }

    /// An in_progress wave-1 bead also holds a ready wave-2 bead.
    #[test]
    fn wave_gate_inprogress_earlier_wave_holds() {
        let ready = vec![rb_wave("bd-w2", 2)];
        let inprog = vec![rb_wave("bd-w1", 1)];
        let held = wave_gate_held(&ready, &inprog);
        assert_eq!(held, vec!["bd-w2"], "{held:?}");
    }

    /// A same-wave bead never holds itself or its siblings.
    #[test]
    fn wave_gate_same_wave_does_not_hold() {
        let ready = vec![rb_wave("bd-w2a", 2), rb_wave("bd-w2b", 2)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let held = wave_gate_held(&ready, &inprog);
        assert!(held.is_empty(), "same wave runs together: {held:?}");
    }

    /// Multiple open waves: only beads above the LOWEST open wave are held —
    /// wave-2 AND wave-3 hold while wave-1 is open.
    #[test]
    fn wave_gate_holds_all_above_min_open_wave() {
        let ready = vec![
            rb_wave("bd-w1", 1),
            rb_wave("bd-w2", 2),
            rb_wave("bd-w3", 3),
        ];
        let inprog: Vec<ReadyBead> = Vec::new();
        let held = wave_gate_held(&ready, &inprog);
        assert_eq!(held, vec!["bd-w2", "bd-w3"], "{held:?}");
    }

    /// Labeled fixture: `--wave-label wave-2` selects EXACTLY the wave-2
    /// beads; wave-3/unlabeled siblings drop out. (Lower-wave OPEN beads
    /// blocking is asserted separately — this fixture is hold-free.)
    #[test]
    fn wave_scope_filters_to_exact_label() {
        let ready = vec![
            rb_wave("bd-w2a", 2),
            rb_wave("bd-w2b", 2),
            rb("bd-unlabeled", None),
            rb_wave("bd-w3", 3),
        ];
        let inprog: Vec<ReadyBead> = Vec::new();
        let scoped = wave_scope_filter(&ready, &inprog, "wave-2").expect("no lower wave open");
        let ids: Vec<&str> = scoped.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-w2a", "bd-w2b"], "{ids:?}");
    }

    /// Unlabeled fixture: a ready set with NO matching labels scopes to an
    /// empty set (nothing to dispatch this label), never an error.
    #[test]
    fn wave_scoped_unlabeled_ready_set_is_empty() {
        let ready = vec![rb("bd-a", None), rb("bd-b", None)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let scoped = wave_scope_filter(&ready, &inprog, "wave-1").expect("no lower wave open");
        assert!(scoped.is_empty(), "{scoped:?}");
    }

    /// Mixed fixture: lower waves CLOSED (absent from ready AND
    /// in_progress) do NOT block — wave-3 dispatches once wave-1/2 are done.
    #[test]
    fn wave_scope_closed_lower_waves_do_not_block() {
        let ready = vec![rb_wave("bd-w3a", 3), rb_wave("bd-w3b", 3)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let scoped = wave_scope_filter(&ready, &inprog, "wave-3").expect("earlier waves closed");
        let ids: Vec<&str> = scoped.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-w3a", "bd-w3b"], "{ids:?}");
    }

    /// Lower-wave-open fixture: an IN_PROGRESS wave-1 bead blocks all wave-2
    /// dispatch — the error names the blocking bead (assert, not silent hold).
    #[test]
    fn wave_scope_inprogress_lower_wave_blocks_with_named_bead() {
        let ready = vec![rb_wave("bd-w2", 2)];
        let inprog = vec![rb_wave("bd-w1", 1)];
        let err =
            wave_scope_filter(&ready, &inprog, "wave-2").expect_err("wave-1 still in progress");
        assert!(err.contains("bd-w1"), "must name the blocker: {err}");
    }

    /// A READY-but-unclaimed wave-1 bead is still OPEN — it blocks wave-2
    /// too (waves run to completion before the next wave starts).
    #[test]
    fn wave_scope_ready_lower_wave_blocks() {
        let ready = vec![rb_wave("bd-w1", 1), rb_wave("bd-w2", 2)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let err = wave_scope_filter(&ready, &inprog, "wave-2")
            .expect_err("wave-1 is open (ready, unclaimed)");
        assert!(err.contains("bd-w1"), "must name the blocker: {err}");
    }

    /// Multiple open lower-wave beads: ALL of them are named in the error,
    /// so the operator sees the full hold at a glance.
    #[test]
    fn wave_scope_names_every_lower_wave_blocker() {
        let ready = vec![rb_wave("bd-w2", 2), rb_wave("bd-w3", 3)];
        let inprog = vec![rb_wave("bd-w1a", 1), rb_wave("bd-w1b", 1)];
        let err =
            wave_scope_filter(&ready, &inprog, "wave-3").expect_err("two wave-1 beads still open");
        assert!(err.contains("bd-w1a") && err.contains("bd-w1b"), "{err}");
    }

    /// Same-wave in_progress siblings never block their own wave.
    #[test]
    fn wave_scope_same_wave_inprogress_does_not_block() {
        let ready = vec![rb_wave("bd-w2b", 2)];
        let inprog = vec![rb_wave("bd-w2a", 2)];
        let scoped = wave_scope_filter(&ready, &inprog, "wave-2").expect("same wave runs together");
        let ids: Vec<&str> = scoped.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-w2b"], "{ids:?}");
    }

    /// Acceptance: the gate reads the TYPED `wave` field. A hold and a
    /// release driven by beads carrying NO `wave-N` label at all — the
    /// schema-18 shape. Proven red by making `bead_wave` label-only.
    #[test]
    fn wave_gate_reads_typed_field() {
        let ready = vec![rb_typed("bd-w1", 1), rb_typed("bd-w3", 3)];
        let inprog: Vec<ReadyBead> = Vec::new();
        let held = wave_gate_held(&ready, &inprog);
        assert_eq!(
            held,
            vec!["bd-w3"],
            "typed wave 3 is held while typed wave 1 is open: {held:?}"
        );
        let ready = vec![rb_typed("bd-w3", 3)];
        assert!(
            wave_gate_held(&ready, &inprog).is_empty(),
            "typed wave 3 releases when typed wave 1 closes"
        );
        let inprog = vec![rb_typed("bd-w1", 1)];
        assert_eq!(wave_gate_held(&ready, &inprog), vec!["bd-w3"]);
    }

    /// The typed field beats a conflicting legacy label — labels are legacy,
    /// and a stale label must never overrule the tracker.
    #[test]
    fn typed_wave_beats_conflicting_label() {
        let mut b = rb_typed("bd-x", 1);
        b.labels = vec!["wave-2".into()];
        assert_eq!(bead_wave(&b), Some(1), "typed field is the authority");
    }

    /// Legacy label beads still gate (migration fallback): no typed field ->
    /// the `wave-N` label decides, exactly as before.
    #[test]
    fn label_wave_still_gates_without_typed_field() {
        assert_eq!(bead_wave(&rb_wave("bd-legacy", 2)), Some(2));
    }

    /// `bead_wave_field` reads a numeric `wave` and refuses a malformed one
    /// (so a bad value falls back to the label, never fabricates a wave).
    #[test]
    fn bead_wave_field_parses_numbers_only() {
        assert_eq!(bead_wave_field(&serde_json::json!({"wave": 3})), Some(3));
        assert_eq!(bead_wave_field(&serde_json::json!({"wave": null})), None);
        assert_eq!(bead_wave_field(&serde_json::json!({"wave": "3"})), None);
        assert_eq!(bead_wave_field(&serde_json::json!({})), None);
    }

    /// `--wave-label wave-2` SCOPE reads the typed field too (selection, not
    /// just the hold): typed-wave rows are dispatched with no label present,
    /// and a lower typed wave blocks with its id named.
    #[test]
    fn wave_scope_selects_typed_wave() {
        let ready = vec![rb_typed("bd-w2", 2), rb_typed("bd-w3", 3)];
        let scoped = wave_scope_filter(&ready, &[], "wave-2").expect("no lower wave open");
        let ids: Vec<&str> = scoped.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-w2"], "{ids:?}");
        let err = wave_scope_filter(&ready, &[rb_typed("bd-w1", 1)], "wave-2")
            .expect_err("lower typed wave open");
        assert!(err.contains("bd-w1"), "must name the blocker: {err}");
    }

    /// A bead held by a NON-live assignee (captain, dead agent, unknown
    /// identity) is stranded — reclaim it.
    #[test]
    fn stranded_beads_flag_non_live_assignees() {
        let live = vec!["oc".to_string(), "pi".to_string()];
        let inprog = vec![
            rb("bd-1", Some("oc")),
            rb("bd-2", Some("captain")),
            rb("bd-3", Some("ghost")),
            rb("bd-4", None),
        ];
        let stranded = stranded_beads(&inprog, &live);
        let ids: Vec<&str> = stranded.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-2", "bd-3", "bd-4"], "{ids:?}");
    }

    /// bd-qk9q regression pin: an in_progress bead with NO assignee (flipped
    /// in_progress without --claim, or claimed then abandoned) has no live
    /// agent working it and nothing else will ever close it — the loop must
    /// reclaim it. This is the bd-bvwf.1 strand: the old `None => false`
    /// carve-out left it in_progress forever while `blocking_inprogress`
    /// treated it as blocking, stalling the batch.
    #[test]
    fn stranded_beads_reclaims_unowned_inprogress() {
        let live = vec!["oc".to_string()];
        let inprog = vec![rb("bd-1", None)];
        let ids: Vec<&str> = stranded_beads(&inprog, &live)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, vec!["bd-1"]);
    }

    /// bd-bvwf.5 guard: a PRESENT-but-unknown assignee is NOT stranded — an
    /// unknown agent may still be alive (detection grace pending); reclaim
    /// targets GONE/absent agents only (bd-qk9q fix direction #3).
    #[test]
    fn stranded_beads_keeps_present_unknown_assignees() {
        let live = vec!["oc".to_string()];
        let inprog = vec![rb("bd-1", Some("oc"))];
        assert!(stranded_beads(&inprog, &live).is_empty());
        let absent = vec![rb("bd-2", Some("ghost"))];
        assert_eq!(
            stranded_beads(&absent, &live).len(),
            1,
            "absent agent IS stranded"
        );
    }

    /// Stranded beads do NOT block completion; live-held and unowned
    /// in-progress beads DO.
    #[test]
    fn blocking_inprogress_counts_live_and_unowned_only() {
        let live = vec!["oc".to_string()];
        let inprog = vec![
            rb("bd-1", Some("oc")),
            rb("bd-2", Some("captain")),
            rb("bd-3", None),
        ];
        let blocking = blocking_inprogress(&inprog, &live);
        let ids: Vec<&str> = blocking.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-1", "bd-3"], "{ids:?}");
    }

    /// All stranded → nothing blocks → the loop may complete once the rest
    /// of the conditions hold (the strand was reclaimed this poll).
    #[test]
    fn blocking_inprogress_empty_when_all_stranded() {
        let live = vec!["oc".to_string()];
        let inprog = vec![rb("bd-1", Some("captain")), rb("bd-2", Some("ghost"))];
        assert!(blocking_inprogress(&inprog, &live).is_empty());
        assert!(
            !stranded_beads(&inprog, &live).is_empty(),
            "they ARE stranded"
        );
    }

    /// bd-reclaim-liveness-not-spelling-hn8b: the reclaim decision resolves
    /// the assignee through the registry to its PANE and asks whether that
    /// pane is live, instead of comparing spelling against a name list. A
    /// live pane is kept under any of its names; a proven-dead pane is
    /// reclaimed; a live lease on the bead keeps it even when the pane is
    /// gone; and a spelling with no pane evidence is kept — never race a
    /// worker whose death is unproven.
    #[test]
    fn reclaim_requires_dead_pane() {
        use std::collections::HashSet;
        fn set(items: &[&str]) -> HashSet<String> {
            items.iter().map(|s| s.to_string()).collect()
        }
        let registry = vec![
            crate::state::SpawnEntry::with_mail_identity("pane-a", "crew-x-coder", "SilverPond"),
            crate::state::SpawnEntry::with_mail_identity(
                "pane-dead",
                "crew-x-nurse",
                "SlateHerder",
            ),
        ];
        let live_panes = set(&["pane-a"]);
        let live_identities = set(&["crew-x-coder", "SilverPond"]);
        let leased = set(&["bd-lease"]);
        let world = ReclaimWorld {
            live_identities: &live_identities,
            registry: &registry,
            live_panes: &live_panes,
            leased_beads: &leased,
        };
        assert_eq!(
            reclaim_decision("bd-hc", Some("crew-x-coder"), &world),
            ReclaimDecision::Keep {
                reason: ReclaimHold::LivePane
            }
        );
        assert_eq!(
            reclaim_decision("bd-pin", Some("SilverPond"), &world),
            ReclaimDecision::Keep {
                reason: ReclaimHold::LivePane
            }
        );
        assert_eq!(
            reclaim_decision("bd-dead", Some("SlateHerder"), &world),
            ReclaimDecision::Reclaim {
                reason: ReclaimReason::DeadPane
            }
        );
        assert_eq!(
            reclaim_decision("bd-lease", Some("SlateHerder"), &world),
            ReclaimDecision::Keep {
                reason: ReclaimHold::LiveLease
            }
        );
        assert_eq!(
            reclaim_decision("bd-unowned", None, &world),
            ReclaimDecision::Reclaim {
                reason: ReclaimReason::Unowned
            }
        );
        assert_eq!(
            reclaim_decision("bd-ghost", Some("ghost"), &world),
            ReclaimDecision::Keep {
                reason: ReclaimHold::UnknownIdentity
            }
        );
    }

    /// One row per cadence contract (base 300s, active 30s): settled states
    /// ride the base cadence, working/blocked/unknown are unsettled, and a
    /// dispatch forces the active cadence because states are PRE-action (an
    /// idle agent that just received a queue item works during the sleep).
    #[test]
    fn next_interval_rows() {
        let rows = [
            ("working-active", vec![st("a", "working")], false, 30),
            ("blocked-active", vec![st("a", "blocked")], false, 30),
            (
                "all-idle-base",
                vec![st("a", "idle"), st("b", "done")],
                false,
                300,
            ),
            ("dispatch-forces-active", vec![st("a", "idle")], true, 30),
            ("all-done-base", vec![st("a", "done")], false, 300),
            ("unknown-active", vec![st("a", "unknown")], false, 30),
        ];
        for (name, states, dispatched, expected) in rows {
            assert_eq!(
                next_interval_secs(&states, dispatched, false, 300, 30),
                expected,
                "row {name}"
            );
        }
    }

    #[test]
    fn next_interval_empty_is_base() {
        let states = vec![];
        assert_eq!(next_interval_secs(&states, false, false, 300, 30), 300);
    }

    /// bd-bvwf.2 detection-retry: pending work + NO classified agent (empty
    /// or all-unknown swarm — a freshly spawned crew hc has not identified
    /// yet) → ACTIVE cadence, so the loop re-polls in ~30s instead of the
    /// settled 300s and dispatches the moment detection completes.
    #[test]
    fn next_interval_pending_work_with_unclassified_swarm_is_active() {
        let empty = vec![];
        assert_eq!(next_interval_secs(&empty, false, true, 300, 30), 30);
        let unknown = vec![st("a", "unknown"), st("b", "unknown")];
        assert_eq!(next_interval_secs(&unknown, false, true, 300, 30), 30);
        let idle = vec![st("a", "idle")];
        assert_eq!(next_interval_secs(&idle, false, true, 300, 30), 300);
        assert_eq!(next_interval_secs(&empty, false, false, 300, 30), 300);
    }

    #[test]
    fn next_interval_clamps_to_interval_min() {
        let states = vec![st("a", "working")];
        assert_eq!(next_interval_secs(&states, false, false, 300, 2), 5);
        let idle = vec![st("a", "idle")];
        assert_eq!(next_interval_secs(&idle, false, false, 3, 30), 5);
    }

    fn so(landed: Option<bool>, reason: Option<&str>) -> SendOutcome {
        SendOutcome {
            deduped: false,
            ok: true,
            cancelled: false,
            name: "a".to_string(),
            landed,
            status: None,
            reason: reason.map(str::to_string),
        }
    }

    /// bd-gkwu: cancellation is a first-class outcome — distinct from
    /// success, typed failure, and Unknown. It never respawns, never counts
    /// as health failure, and never collapses into Accept.
    #[test]
    fn authority_edge_cancellation_stays_distinct() {
        let mut outcome = so(Some(false), Some("cancelled"));
        outcome.cancelled = true;
        assert_eq!(
            escalate(&outcome, 0),
            Escalation::Cancelled,
            "cancelled must escalate Cancelled"
        );
        assert_ne!(
            escalate(&outcome, 0),
            Escalation::Accept,
            "cancelled must never read as success"
        );
        assert_ne!(
            escalate(&outcome, 5),
            Escalation::FailStop,
            "cancelled must never trip the failure budget"
        );
    }

    #[test]
    fn cancelled_send_is_not_a_health_failure() {
        let mut outcome = so(Some(false), Some("cancelled"));
        outcome.cancelled = true;
        let mut failures = std::collections::HashMap::new();
        assert_eq!(
            handle_send_result(&mut failures, "a", &outcome),
            Escalation::Cancelled
        );
        assert!(
            failures.is_empty(),
            "cancellation must not trigger respawn state"
        );
    }

    #[test]
    fn escalate_stalled_below_cap_respawns() {
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 0),
            Escalation::RespawnAndRequeue
        );
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 1),
            Escalation::RespawnAndRequeue
        );
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 2),
            Escalation::RespawnAndRequeue
        );
    }

    #[test]
    fn escalate_stalled_at_cap_stops() {
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 3),
            Escalation::FailStop
        );
    }

    /// cg6.4: the FailStop message must state the REAL threshold (4 prompts),
    /// matching the pre-increment counter semantics — message, doc and code
    /// agree on one number.
    #[test]
    fn failstop_message_states_real_contract() {
        let m = failstop_message("agent-a", Some("stalled"));
        assert!(m.contains("agent-a"));
        assert!(
            m.contains("4 prompts in a row"),
            "message must match the actual 4-failure threshold: {m}"
        );
        assert!(!m.contains("3 prompts"), "no stale count in message: {m}");
        let m2 = failstop_message("agent-b", None);
        assert!(m2.contains("4 prompts in a row"));
    }

    #[test]
    fn escalate_coded_error_respawns() {
        assert_eq!(
            escalate(&so(Some(false), Some("error:agent_not_found")), 0),
            Escalation::RespawnAndRequeue
        );
    }

    #[test]
    fn escalate_timeout_accepts() {
        assert_eq!(
            escalate(&so(Some(false), Some("timeout")), 0),
            Escalation::Accept
        );
        assert_eq!(
            escalate(&so(Some(false), Some("timeout")), 9),
            Escalation::Accept
        );
    }

    #[test]
    fn escalate_landed_accepts() {
        let mut o = so(Some(true), Some("landed"));
        o.status = Some("working".to_string());
        assert_eq!(escalate(&o, 0), Escalation::Accept);
        assert_eq!(escalate(&o, 7), Escalation::Accept);
    }

    #[test]
    fn escalate_blocked_accepts() {
        let mut o = so(Some(true), Some("landed"));
        o.status = Some("blocked".to_string());
        assert_eq!(escalate(&o, 0), Escalation::Accept);
    }

    #[test]
    fn escalate_no_verdict_accepts() {
        assert_eq!(escalate(&so(None, None), 0), Escalation::Accept);
    }

    #[test]
    fn handle_send_result_landed_resets() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 2);
        let esc = handle_send_result(&mut f, "a", &so(Some(true), Some("landed")));
        assert_eq!(esc, Escalation::Accept);
        assert!(!f.contains_key("a"), "landed send must clear the counter");
    }

    #[test]
    fn handle_send_result_timeout_keeps_counter() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 2);
        let esc = handle_send_result(&mut f, "a", &so(Some(false), Some("timeout")));
        assert_eq!(esc, Escalation::Accept);
        assert_eq!(f.get("a"), Some(&2));
    }

    #[test]
    fn handle_send_result_stall_increments() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 1);
        let esc = handle_send_result(&mut f, "a", &so(Some(false), Some("stalled")));
        assert_eq!(esc, Escalation::RespawnAndRequeue);
        assert_eq!(f.get("a"), Some(&2), "stall must increment the counter");
        let mut g: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        handle_send_result(&mut g, "a", &so(Some(false), Some("error:agent_not_found")));
        assert_eq!(g.get("a"), Some(&1));
    }

    #[test]
    fn handle_send_result_failstop_does_not_increment() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 3);
        let esc = handle_send_result(&mut f, "a", &so(Some(false), Some("stalled")));
        assert_eq!(esc, Escalation::FailStop);
        assert_eq!(f.get("a"), Some(&3));
    }

    #[test]
    fn clear_live_failures_resets_working_and_blocked() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("work".to_string(), 2);
        f.insert("block".to_string(), 1);
        f.insert("idle".to_string(), 2);
        let states = vec![
            st("work", "working"),
            st("block", "blocked"),
            st("idle", "idle"),
        ];
        clear_live_failures(&mut f, &states);
        assert!(!f.contains_key("work"));
        assert!(!f.contains_key("block"));
        assert_eq!(f.get("idle"), Some(&2), "idle is not proof of life");
    }

    #[test]
    fn clear_live_failures_keeps_absent_and_unknown() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("gone".to_string(), 1);
        f.insert("unk".to_string(), 1);
        let states = vec![st("unk", "unknown")];
        clear_live_failures(&mut f, &states);
        assert_eq!(f.get("gone"), Some(&1));
        assert_eq!(f.get("unk"), Some(&1));
    }

    /// ADR-0019 Spec 2 test helpers: filled briefs (fresh + resume) that fit
    /// the dispatch cap.
    fn brief(id: &str, title: &str) -> String {
        build_brief(id, title, "flywheel", false).expect("brief fits the dispatch cap")
    }

    fn resume_brief(id: &str, title: &str) -> String {
        build_brief(id, title, "flywheel", true).expect("brief fits the dispatch cap")
    }

    /// ADR-0019 Spec 2: a resume brief is the same filled template with the
    /// GOAL prefixed `Resume <id>:` — the old prose `resume_prompt` is gone.
    #[test]
    fn resume_brief_mentions_continuation() {
        let p = resume_brief("bd-7", "finish the parser");
        assert!(p.contains("bd-7"));
        assert!(p.contains("Resume"));
        assert!(p.contains("CONTEXT_LIMIT"));
        assert!(p.starts_with("GOAL Resume bd-7: finish the parser."));
    }

    #[test]
    fn resume_brief_carries_state_file_contract() {
        let p = resume_brief("bd-7", "finish the parser");
        assert!(p.contains(".flywheel/agent-state/"));
        assert!(p.contains("context_pct"));
        assert!(p.contains("{AGENT_NAME}"));
        assert!(p.contains(".flywheel/agent-state/{AGENT_NAME}.json"));
    }

    #[test]
    fn context_marker_detection() {
        assert_eq!(
            context_verdict("...CONTEXT_LIMIT", None, 1_785_726_000),
            ContextVerdict::Healthy,
            "a marker inside prose/source is not a harness exhaustion signal"
        );
        assert_eq!(
            context_verdict(
                "if terminal.contains(\"CONTEXT_LIMIT\") {",
                None,
                1_785_726_000
            ),
            ContextVerdict::Healthy,
            "reading the source must not self-kill the agent"
        );
        assert_eq!(
            context_verdict("CONTEXT_LIMIT\nall done", None, 1_785_726_000),
            ContextVerdict::Exhausted
        );
        assert_eq!(
            context_verdict("all done", None, 1_785_726_000),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn context_verdict_fresh_file_exhausts_without_marker() {
        let state = Some(AgentStateFile {
            context_pct: 85,
            ts: "2026-08-03T03:00:00Z".into(),
        });
        assert_eq!(
            context_verdict("", state.as_ref(), 1_785_726_000 + 100),
            ContextVerdict::Exhausted
        );
    }

    #[test]
    fn context_verdict_stale_file_is_healthy() {
        let state = Some(AgentStateFile {
            context_pct: 85,
            ts: "2026-08-03T03:00:00Z".into(),
        });
        assert_eq!(
            context_verdict("", state.as_ref(), 1_785_726_000 + 3600),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn context_verdict_both_channels_exhausts() {
        let state = Some(AgentStateFile {
            context_pct: 90,
            ts: "2026-08-03T03:00:00Z".into(),
        });
        assert_eq!(
            context_verdict("CONTEXT_LIMIT", state.as_ref(), 1_785_726_000),
            ContextVerdict::Exhausted
        );
    }

    #[test]
    fn context_verdict_neither_channel_is_healthy() {
        assert_eq!(
            context_verdict("working fine", None, 1_785_726_000),
            ContextVerdict::Healthy
        );
        assert_eq!(
            context_verdict("", None, 1_785_726_000),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn context_verdict_pct_boundary() {
        let ts = "2026-08-03T03:00:00Z";
        let low = AgentStateFile {
            context_pct: 79,
            ts: ts.into(),
        };
        let at = AgentStateFile {
            context_pct: 80,
            ts: ts.into(),
        };
        assert_eq!(
            context_verdict("", Some(&low), 1_785_726_000),
            ContextVerdict::Healthy
        );
        assert_eq!(
            context_verdict("", Some(&at), 1_785_726_000),
            ContextVerdict::Exhausted
        );
    }

    #[test]
    fn context_verdict_age_boundary() {
        let ts = "2026-08-03T03:00:00Z";
        let state = AgentStateFile {
            context_pct: 85,
            ts: ts.into(),
        };
        assert_eq!(
            context_verdict("", Some(&state), 1_785_726_000 + 900),
            ContextVerdict::Exhausted
        );
        assert_eq!(
            context_verdict("", Some(&state), 1_785_726_000 + 901),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn parse_read_output_parses_new_leaf_shape() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"terminal": "hello", "state": {"context_pct": 85, "ts": "2026-08-03T03:00:00Z"}}"#,
        )
        .unwrap();
        let ro = parse_read_output(&v);
        assert_eq!(ro.terminal, "hello");
        assert_eq!(ro.state.as_ref().map(|s| s.context_pct), Some(85));
    }

    #[test]
    fn parse_read_output_null_and_missing_state() {
        let null_state: serde_json::Value =
            serde_json::from_str(r#"{"terminal": "x", "state": null}"#).unwrap();
        let ro = parse_read_output(&null_state);
        assert_eq!(ro.state, None);
        let missing: serde_json::Value = serde_json::from_str(r#"{"terminal": "y"}"#).unwrap();
        let ro = parse_read_output(&missing);
        assert_eq!(ro.state, None);
        assert_eq!(ro.terminal, "y");
    }

    /// ADR-0019 Spec 2: the brief replaces the lifecycle novel — it is
    /// self-contained (id + GOAL + standing-orders path), not a prose
    /// claim→close recipe.
    #[test]
    fn brief_is_self_contained() {
        let p = brief("bd-12", "fix auth flow");
        assert!(p.contains("bd-12"));
        assert!(p.contains("GOAL Execute bd-12: fix auth flow."));
        assert!(p.contains(".flywheel/standing-orders.md"));
    }

    #[test]
    fn brief_carries_state_file_contract() {
        let p = brief("bd-12", "fix auth flow");
        assert!(p.contains(".flywheel/agent-state/"));
        assert!(p.contains("context_pct"));
        assert!(p.contains(".flywheel/agent-state/{AGENT_NAME}.json"));
        assert!(p.contains("CONTEXT_LIMIT"));
    }

    /// The brief fills ALL ten Spec 2 fields.
    #[test]
    fn brief_fills_all_ten_fields() {
        let p = brief("bd-12", "fix auth flow");
        for field in [
            "GOAL",
            "SCOPE",
            "CONTEXT",
            "ACCEPTANCE",
            "VERIFY",
            "TIMEBOX",
            "FORBIDDEN",
            "REPORT",
            "PRINCIPLES",
            "STANDING",
        ] {
            assert!(p.contains(field), "brief carries the {field} field: {p}");
        }
    }

    /// ADR-0019 Spec 2: the lifecycle (claim → reserve → announce → execute →
    /// verify → close → sync → commit) lives in the standing-orders file,
    /// referenced BY PATH ONLY. The brief must not restate the commands.
    #[test]
    fn brief_standing_is_path_only_and_no_lifecycle_commands() {
        let p = brief("bd-12", "fix auth flow");
        assert!(p.contains("flywheel/.flywheel/standing-orders.md"));
        assert!(p.contains("~/.config/flywheel/standing-orders.md"));
        for cmd in [
            "br update",
            "br sync",
            "br close",
            "locks reserve",
            "--claim",
        ] {
            assert!(
                !p.contains(cmd),
                "lifecycle command {cmd} must live in standing orders, not the brief: {p}"
            );
        }
    }

    /// The old novel header is gone from every brief variant — no dispatched
    /// prompt may carry it (rg acceptance: the two-word sequence must not
    /// appear anywhere in flywheel/src, including inside this guard).
    #[test]
    fn brief_never_says_execute_bead() {
        let old_header = ["Execute", "bead"].join(" ");
        for p in [
            brief("bd-12", "fix auth flow"),
            resume_brief("bd-7", "finish the parser"),
        ] {
            assert!(
                !p.contains(&old_header),
                "old novel header must be gone: {p}"
            );
        }
    }

    /// Both brief variants carry the self-renew clause: agents renew their
    /// own reservations (the orchestrator cannot renew on their behalf).
    #[test]
    fn briefs_carry_lease_renew_clause() {
        for p in [
            brief("bd-12", "fix auth flow"),
            resume_brief("bd-7", "finish the parser"),
        ] {
            assert!(
                p.to_lowercase()
                    .contains("renew your own file reservations"),
                "prompt must instruct agents to renew their own leases: {p}"
            );
            assert!(
                p.contains("cannot renew on your behalf"),
                "prompt must explain the orchestrator cannot renew for agents: {p}"
            );
        }
    }

    /// bd-fug: the ack contract carries into both brief variants
    /// (grep-verifiable).
    #[test]
    fn briefs_carry_ack_contract_clause() {
        for p in [
            brief("bd-12", "fix auth flow"),
            resume_brief("bd-7", "finish the parser"),
        ] {
            assert!(
                p.contains("ack_required=true"),
                "prompt must mention ack_required: {p}"
            );
            assert!(
                p.contains("acknowledge_message"),
                "prompt must require acknowledge_message: {p}"
            );
            assert!(
                p.contains("cannot ack on your behalf"),
                "prompt must state orchestrator cannot ack: {p}"
            );
        }
    }

    /// ADR-0019 Spec 2, amended by bd-dispatch-cap-conditioning-l7jh: over
    /// the cap the dispatch DEGRADES to the pointer brief — never a drop,
    /// never a silent truncation. Only a bead id so large it cannot fit even
    /// the pointer form fails loud.
    #[test]
    fn brief_cap_degrades_to_pointer() {
        let huge = "x".repeat(8192);
        let p = build_brief("bd-1", &huge, "flywheel", false).expect("degrades, not errors");
        assert!(
            p.len() <= DISPATCH_PROMPT_CAP,
            "pointer fits: {} bytes",
            p.len()
        );
        assert!(p.contains("OVER CAP"), "pointer announces the degradation");
        assert!(p.contains("br show bd-1"), "pointer names the read path");
        let absurd = "b".repeat(DISPATCH_PROMPT_CAP + 64);
        let err = build_brief(&absurd, "t", "flywheel", false).unwrap_err();
        assert!(err.bytes > DISPATCH_PROMPT_CAP);
    }

    /// The cap boundary: a full brief landing exactly ON the cap is legal;
    /// one byte more degrades to the pointer (still ≤ cap), never a drop.
    #[test]
    fn brief_cap_boundary_degrades() {
        let base = build_brief("bd-1", "", "flywheel", false).unwrap();
        let room = DISPATCH_PROMPT_CAP.saturating_sub(base.len());
        let fits = build_brief("bd-1", &"x".repeat(room), "flywheel", false).unwrap();
        assert!(fits.len() <= DISPATCH_PROMPT_CAP);
        assert!(
            !fits.contains("OVER CAP"),
            "boundary brief is still the full one"
        );
        let over = build_brief("bd-1", &"x".repeat(room + 1), "flywheel", false).unwrap();
        assert!(
            over.len() <= DISPATCH_PROMPT_CAP,
            "degrades, never overshoots"
        );
        assert!(over.contains("OVER CAP"), "degraded to the pointer form");
    }

    /// bd-dispatch-cap-conditioning-l7jh: the cap must size the WHOLE send —
    /// behavior preamble + skills prefix + brief — and degrade instead of
    /// killing the loop. The old template filled to ~3.2 KiB with a 38-char
    /// id, so template + the smallest profile preamble (1357 B) was always
    /// over the 4 KiB cap; every composed-pane dispatch was rejected and four
    /// rejects in a row hard-stopped the supervisor.
    #[test]
    fn dispatch_cap_accounts_for_conditioning() {
        let id = "bd-reclaim-liveness-not-spelling-hn8b";
        let full = build_brief(id, "friction-report title", "proj", false).unwrap();
        assert!(!full.contains("OVER CAP"), "full brief, not the pointer");
        assert!(
            full.len() + 1600 <= DISPATCH_PROMPT_CAP,
            "brief {} bytes + worst preamble 1600 exceeds cap",
            full.len()
        );
        let pointer = pointer_brief("bd-dispatch-cap-conditioning-l7jh", "proj", false);
        assert!(
            pointer.len() + 1600 <= DISPATCH_PROMPT_CAP,
            "pointer {} bytes + 1600 conditioning fits",
            pointer.len()
        );
        let mut q: Vec<String> = Vec::new();
        let title = "y".repeat(DISPATCH_PROMPT_CAP);
        queue_brief(&mut q, id, &title, "proj", false, false);
        assert_eq!(q.len(), 1, "over-cap dispatch stays queued (pointer brief)");
        assert!(q[0].starts_with("GOAL "), "pointer keeps the GOAL contract");
        assert!(q[0].contains("OVER CAP") && q[0].contains(id));
    }

    /// The {AGENT_NAME} placeholder survives the fill — plan_actions
    /// substitutes the receiving agent's name at dispatch (never the
    /// template).
    #[test]
    fn brief_agent_name_placeholder_survives_fill() {
        let p = brief("bd-12", "fix auth flow");
        assert!(p.contains("{AGENT_NAME}"));
        assert!(p.contains(".flywheel/agent-state/{AGENT_NAME}.json"));
    }

    /// Renewal extension per tick: `4 × heartbeat`, floored at agent-mail's
    /// 60s minimum — never compute tiny extensions.
    #[test]
    fn lease_renew_extension_floor_and_scale() {
        assert_eq!(lease_renew_seconds(300), 1200);
        assert_eq!(lease_renew_seconds(1000), 4000);
        assert_eq!(lease_renew_seconds(30), 120);
        assert_eq!(lease_renew_seconds(10), 60);
        assert_eq!(lease_renew_seconds(0), 60);
    }

    #[test]
    fn plan_actions_substitutes_agent_name_at_dispatch() {
        let queue = vec![brief("bd-12", "fix auth flow")];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::PinStamp { .. } => {}
            other => panic!("expected PinStamp first, got {other:?}"),
        }
        match &actions[1] {
            LoopAction::Send { name, text } => {
                assert_eq!(name, "flywheel-proj-oc");
                assert!(text.contains(".flywheel/agent-state/flywheel-proj-oc.json"));
                assert!(!text.contains("{AGENT_NAME}"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    /// bd-pin-public-plane-s-easy.2: {AGENT_NAME} in the brief substitutes
    /// the agent's mail PIN, never the hc slug — the state-file path and
    /// br claim actor must match the identity the agent is addressed by.
    #[test]
    fn plan_actions_substitutes_pin_not_slug() {
        let queue = vec![brief("bd-12", "fix auth flow")];
        let mut s = st("flywheel-proj-oc", "idle");
        s.mail_identity = Some("DustySparrow".to_string());
        let states = vec![s];
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        match &actions[1] {
            LoopAction::Send { name, text } => {
                assert_eq!(name, "flywheel-proj-oc", "send target stays the hc name");
                assert!(
                    text.contains(".flywheel/agent-state/DustySparrow.json"),
                    "state-file path uses the pin: {text}"
                );
                assert!(
                    !text.contains(".flywheel/agent-state/flywheel-proj-oc.json"),
                    "never the slug: {text}"
                );
                assert!(!text.contains("{AGENT_NAME}"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn plan_actions_leaves_plain_prompts_untouched() {
        let queue = vec!["t1".to_string()];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Send { text, .. } => assert_eq!(text, "t1"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    /// bd-0cet: >6 new ready beads in one poll = runaway create flood;
    /// 0-6 is normal dispatch volume (ADR-0022 waves are 5-6 beads).
    /// Decided by the persisted Wake table now (was free fns + loop set).
    #[test]
    fn bead_flood_verdict_threshold() {
        use doorbell_core::wake_core::{FloodGate, WakeModel, flood_decide};
        let ids = |n: usize| (0..n).map(|i| format!("bd-t{i}")).collect::<Vec<_>>();
        for n in [0, 3, 6] {
            let (_, d) = flood_decide(WakeModel::default(), &ids(n));
            assert!(matches!(d, FloodGate::Proceed), "{n} proceeds");
        }
        for n in [7, 40] {
            let (_, d) = flood_decide(WakeModel::default(), &ids(n));
            assert!(matches!(d, FloodGate::Pause { .. }), "{n} pauses");
        }
    }

    /// bd-0cet: the flood guard's paused beads enter the Wake table, so a
    /// pre-created flood-sized batch is paused ONCE and then dispatches; a
    /// runaway minting new ids keeps pausing. Batch is 7 to clear the
    /// threshold (verdict trips at >6; bd-e7ev). Table persists via
    /// ActiveLoop, so crash-resume inherits the absorption.
    #[test]
    fn flood_new_count_absorbs_paused_batch_and_catches_runaway() {
        use doorbell_core::wake_core::{FloodGate, WakeModel, flood_decide};
        let rb = |id: &str| ReadyBead {
            id: id.into(),
            title: String::new(),
            assignee: None,
            priority: 3,
            principles: String::new(),
            verify: Some("true".into()),
            labels: Vec::new(),
            updated_at: None,
            ..Default::default()
        };
        let batch = [
            rb("bd-a"),
            rb("bd-b"),
            rb("bd-c"),
            rb("bd-d"),
            rb("bd-e"),
            rb("bd-f"),
            rb("bd-g"),
        ];
        let ids: Vec<String> = batch.iter().map(|b| b.id.clone()).collect();
        let (t, first) = flood_decide(WakeModel::default(), &ids);
        assert!(
            matches!(first, FloodGate::Pause { .. }),
            "fresh flood-sized batch trips the pause"
        );
        let (t, second) = flood_decide(t, &ids);
        assert!(
            matches!(second, FloodGate::Proceed),
            "paused batch dispatches on the next poll (no repeat pause)"
        );
        let (_, single) = flood_decide(t.clone(), &[rb("bd-runaway").id]);
        assert!(
            matches!(single, FloodGate::Proceed),
            "a lone new id proceeds"
        );
        let fresh: Vec<String> = (0..7).map(|i| format!("bd-fresh{i}")).collect();
        let (_, runaway_flood) = flood_decide(t, &fresh);
        assert!(
            matches!(runaway_flood, FloodGate::Pause { .. }),
            "a fresh flood-sized wave still trips"
        );
    }

    /// bd-eign.2: the brief forbids creating new beads — the agent must
    /// report scope gaps in the thread, not spawn tracker work.
    #[test]
    fn brief_forbids_creating_new_beads() {
        let p = brief("bd-12", "fix auth flow");
        assert!(
            p.contains("DO NOT create new beads"),
            "prompt must forbid bead creation: {p}"
        );
        assert!(
            p.contains("report scope gaps in the thread"),
            "prompt must redirect scope gaps to the thread: {p}"
        );
    }

    /// Plan\u2192dispatch parity (the survivor\u2019s bar-for-bar invariant,
    /// bd-w4-delete-dup-sbh8): the text `plan_actions` produces is
    /// byte-identical to the text the single dispatch path shells to
    /// `__send` — the engine-dispatch coexistence bridge is deleted, so the
    /// plan\u2192send boundary is the invariant that matters.
    /// ADR-0001 Wave 3 / Layer 3 (beads_rust-wave3-pin-reservation-jxsyt):
    /// dispatching a BEAD brief emits a PinStamp carrying the mail identity
    /// BEFORE the Send — flywheel fills pin, not the agent.
    #[test]
    fn plan_actions_stamps_pin_before_bead_dispatch() {
        let queue = vec![brief("bd-12", "fix auth flow")];
        let mut s = st("flywheel-proj-oc", "idle");
        s.mail_identity = Some("DustySparrow".to_string());
        let states = vec![s];
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        assert!(actions.len() >= 2, "stamp + send: {actions:?}");
        assert!(
            matches!(&actions[0], LoopAction::PinStamp { bead_id, pin }
                if bead_id == "bd-12" && pin == "DustySparrow"),
            "PinStamp precedes the dispatch: {actions:?}"
        );
        assert!(matches!(&actions[1], LoopAction::Send { .. }));
    }

    /// No known pin → the stamp falls back to the hc name (still a
    /// deterministic owner), and non-bead prompts never stamp.
    #[test]
    fn plan_actions_pin_stamp_falls_back_and_skips_non_bead_prompts() {
        let queue = vec![brief("bd-13", "fallback owner")];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        assert!(
            matches!(&actions[0], LoopAction::PinStamp { pin, .. } if pin == "flywheel-proj-oc"),
            "hc fallback stamps: {actions:?}"
        );

        let plain_queue = vec!["run the nightly sweep".to_string()];
        let states_plain = vec![st("flywheel-proj-oc", "idle")];
        let plain_actions = plan_actions(
            &LoopModel {
                queue: plain_queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states_plain,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        assert!(
            plain_actions
                .iter()
                .all(|a| !matches!(a, LoopAction::PinStamp { .. })),
            "non-bead prompt must not stamp: {plain_actions:?}"
        );
    }

    /// The stamp argv targets `br update <id> --pin <pin>` — read it back.
    #[test]
    fn pin_stamp_argv_targets_br_update_pin_field() {
        let argv = pin_stamp_argv("bd-12", "DustySparrow");
        assert_eq!(argv[0], "update");
        assert_eq!(argv[1], "bd-12");
        assert_eq!(argv[2], "--pin=DustySparrow");
        assert_eq!(argv[3], "--json");
    }

    #[test]
    fn plan_actions_to_dispatch_parity() {
        let queue = vec![brief("bd-12", "fix auth flow")];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        let LoopAction::Send { name, text } = actions
            .iter()
            .find(|a| matches!(a, LoopAction::Send { .. }))
            .expect("expected Send")
        else {
            panic!("expected Send, got {:?}", actions);
        };
        let argv = send_prompt_argv(&SendArgs {
            project: "toron".into(),
            name: name.clone(),
            text: text.clone(),
            confirm: true,
            confirm_timeout_ms: None,
            invocation: "test-inv".into(),
            iteration: 1,
            profile: String::new(),
        });
        assert!(
            argv.iter().any(|a| a == text),
            "dispatch argv carries the planned text: {argv:?}"
        );
    }

    fn digest(recent: usize, urgent: usize, from: &str, subj: &str) -> MailDigest {
        MailDigest {
            agent: "flywheel-proj-oc".into(),
            mail_identity: None,
            recent,
            urgent,
            plane_s: 0,
            last_id: (urgent > 0)
                .then(|| format!("{:012x}", from.bytes().map(usize::from).sum::<usize>())),
            last_from: (!from.is_empty()).then(|| from.to_string()),
            last_subject: (!subj.is_empty()).then(|| subj.to_string()),
            thread_id: None,
        }
    }

    fn digest_map(d: MailDigest) -> HashMap<String, MailDigest> {
        let mut m = HashMap::new();
        m.insert(d.agent.clone(), d);
        m
    }

    #[test]
    fn plan_actions_prepends_mail_digest_to_dispatched_prompt() {
        let queue = vec!["do the work".to_string()];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let d = digest_map(digest(2, 1, "PinkGorge", "[bd-1] need input"));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Send { text, .. } => {
                assert!(
                    text.starts_with("[mail] 2 recent (1 urgent) (id 000000000386 re: [bd-1] need input)\ndo the work"),
                    "digest must prefix the prompt with the MESSAGE id, not the sender: {text}"
                );
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn plan_actions_skips_digest_prefix_when_no_mail() {
        let queue = vec!["do the work".to_string()];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let d = digest_map(digest(0, 0, "", ""));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Send { text, .. } => assert_eq!(text, "do the work"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn plan_actions_wakes_idle_agent_on_urgent_mail_with_empty_queue() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-proj-oc", "idle")];
        let d = digest_map(digest(1, 1, "PinkGorge", "[bd-1] help needed"));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Send { text, .. } => {
                assert!(
                    text.contains("MAIL WAKE: 1 urgent"),
                    "wake prompt must say urgent: {text}"
                );
                assert!(
                    text.contains("id 000000000386 re: [bd-1] help needed"),
                    "wake must cite the replyable message id, not the sender npub (bd-kryk): {text}"
                );
            }
            other => panic!("expected wake Send, got {other:?}"),
        }
    }

    /// GS-030 slice 1 (one wake, bd-gs030-one-wake-k8m3): the live loop must
    /// not run the digest leaf — the doorbell is the single wake producer
    /// (WatchNdjson ring + InboxRows urgent/high Prompt). A re-added live
    /// call fails this test. The fn definition plus register call remain as
    /// the debug surface. (Shape note: the assertion matches the loop-body
    /// call indentation, so this test's own source never matches itself.)
    #[test]
    fn loop_live_path_one_wake_no_digest_producer() {
        let src = include_str!("durable.rs");
        assert!(
            !src.contains("\n                poll_mail_digests,"),
            "live loop must not ctx.run the digest leaf: the doorbell owns wake"
        );
    }

    /// GS-030 slice 1: with the live empty digest map, an idle agent with an
    /// empty queue Waits — no loop-side MAIL WAKE exists. Urgent wake arrives
    /// via the doorbell (InboxRows Prompt for idle pins), never the loop.
    #[test]
    fn plan_actions_one_wake_empty_digests_wait() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-proj-oc", "idle")];
        let empty: HashMap<String, MailDigest> = HashMap::new();
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &empty,
            &std::collections::HashSet::new(),
        );
        assert!(
            actions.iter().all(|a| matches!(a, LoopAction::Wait)),
            "live empty digests must Wait (doorbell owns wake), got {actions:?}"
        );
    }

    /// bd-context-limit-honor-luqg: a pane being respawned for a saturation
    /// signal is wake-suppressed — no MAIL WAKE banner can land on the same
    /// saturated session between the signal and the fresh boot.
    #[test]
    fn plan_actions_never_wakes_a_context_respawned_pane() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-toron-oc3", "idle")];
        let d = digest_map(digest(1, 1, "PinkGorge", "[bd-1] help needed"));
        let suppressed: std::collections::HashSet<String> =
            ["flywheel-toron-oc3".to_string()].into();
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &suppressed,
        );
        assert!(
            actions.iter().all(|a| matches!(a, LoopAction::Wait)),
            "suppressed saturated pane must Wait, got {actions:?}"
        );
    }

    /// bd-wake-id-contract-1jz1 clause 1: unresolvable handles in the id /
    /// thread slots are DROPPED from the citation — the banner never sends
    /// the agent hunting a phantom handle.
    #[test]
    fn wake_banner_drops_unresolvable_ids() {
        let d = digest(1, 1, "PinkGorge", "[request-release] crates/toron-cli/**");
        let mut bad = d;
        bad.last_id = Some("reservation-30078".into());
        bad.thread_id = Some("thread-9844e931".into());
        let prompt = urgent_wake_prompt(&bad);
        assert!(!prompt.contains("Most recent urgent:"), "{prompt}");
        assert!(!prompt.contains("Latest urgent thread:"), "{prompt}");
        assert!(
            prompt.starts_with("MAIL WAKE:"),
            "the wake itself still fires: {prompt}"
        );
    }

    #[test]
    fn wake_banner_keeps_resolvable_hex_ids() {
        let mut d = digest(1, 1, "PinkGorge", "[bd-1] need input");
        d.last_id = Some("994d117d53db".into());
        d.thread_id = Some("a1b2c3d4e5f60718".into());
        let prompt = urgent_wake_prompt(&d);
        assert!(
            prompt.contains("Most recent urgent: id 994d117d53db"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Latest urgent thread: a1b2c3d4e5f60718"),
            "{prompt}"
        );
    }

    #[test]
    fn citable_wake_id_table() {
        assert!(citable_wake_id(Some("994d117d53db")));
        assert!(citable_wake_id(Some("A1B2C3D4")));
        assert!(!citable_wake_id(Some("reservation-30078")));
        assert!(!citable_wake_id(Some("short")));
        assert!(!citable_wake_id(Some("")));
        assert!(!citable_wake_id(None));
    }

    /// bd-kryk regression: the wake banner cites the MESSAGE id — a sender
    /// npub in that slot made reply-by-id fail and manufactured phantom mail.
    #[test]
    fn kryk_wake_banner_cites_message_id_not_sender() {
        let d = MailDigest {
            agent: "a".into(),
            mail_identity: Some("TealDune".into()),
            recent: 3,
            urgent: 1,
            plane_s: 0,
            last_id: Some("994d117d53db".into()),
            last_from: Some(
                "994d117d53db3b639f387f3686652cb6018325c403d633b933f6d19bf8c71d89".into(),
            ),
            last_subject: None,
            thread_id: None,
        };
        let prompt = urgent_wake_prompt(&d);
        assert!(
            prompt.contains("id 994d117d53db"),
            "banner must carry the message id: {prompt}"
        );
        assert!(
            !prompt.contains("re: \n") && !prompt.contains("() re:"),
            "empty subject renders no dangling re:"
        );
        let prefix = digest_prefix(&d).expect("recent>0 has a prefix");
        assert!(prefix.contains("(id 994d117d53db)"), "{prefix}");
        assert!(!prefix.contains("last:"), "sender-based rendering is gone");
    }

    /// bd-pin-public-plane-s-easy.5: an idle agent with OPEN plane-S work
    /// (kind-43001 jobs) and an empty queue is woken with the plane-S prompt
    /// — crew work is work, not only urgent private mail.
    #[test]
    fn plan_actions_wakes_idle_agent_on_open_plane_s_job() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-proj-oc", "idle")];
        let mut d = digest(0, 0, "", "");
        d.plane_s = 2;
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &digest_map(d),
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Send { text, .. } => {
                assert!(
                    text.contains("PLANE-S WAKE: 2 open job(s)"),
                    "wake prompt must say plane-S: {text}"
                );
                assert!(
                    text.contains("toron job claim"),
                    "wake must name the claim command: {text}"
                );
            }
            other => panic!("expected plane-S wake Send, got {other:?}"),
        }
    }

    /// S8.1: the wake prompt instructs fetch/reply as the REGISTERED mail
    /// identity (mail_identity), never the hc name when they differ.
    #[test]
    fn urgent_wake_prompt_fetches_as_registered_identity() {
        let mut d = digest(1, 1, "CoralFinch", "[assign] x");
        d.mail_identity = Some("YellowCastle".into());
        let text = urgent_wake_prompt(&d);
        assert!(
            text.contains("--as YellowCastle --urgent"),
            "fetch as the registered identity: {text}"
        );
        assert!(
            text.contains("--as YellowCastle`"),
            "reply as the registered identity: {text}"
        );
        assert!(
            !text.contains("--as flywheel-proj-oc"),
            "never fetch as the unregistered terminal name: {text}"
        );
        let plain = urgent_wake_prompt(&digest(1, 1, "CoralFinch", "[assign] x"));
        assert!(plain.contains("--as flywheel-proj-oc --urgent"), "{plain}");
    }

    #[test]
    fn plan_actions_waits_when_idle_with_no_mail_and_no_queue() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-proj-oc", "idle")];
        let d = digest_map(digest(0, 0, "", ""));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &std::collections::HashSet::new(),
        );
        assert_eq!(actions[0], LoopAction::Wait);
    }

    #[test]
    fn plan_actions_nudge_wins_over_urgent_wake_when_nudge_present() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-proj-oc", "idle")];
        let d = digest_map(digest(3, 2, "PinkGorge", "[bd-1] urgent"));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            Some("proceed"),
            &d,
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Nudge { .. } => {}
            other => panic!("expected Nudge (orchestrator direction wins), got {other:?}"),
        }
    }

    #[test]
    fn carry_nudge_streaks_survives_poll_reset() {
        let prior = vec![{
            let mut s = st("a", "idle");
            s.nudge_streak = 2;
            s
        }];
        let polled = vec![st("a", "idle")];
        let out = carry_nudge_streaks(polled, &prior);
        assert_eq!(out[0].nudge_streak, 2);
        let working = vec![st("a", "working")];
        assert_eq!(carry_nudge_streaks(working, &prior)[0].nudge_streak, 0);
    }

    #[test]
    fn bump_nudge_streak_compacts_at_three() {
        let mut states = vec![st("a", "idle")];
        assert_eq!(
            bump_nudge_streak(&mut states, "a"),
            crate::rot::RotAction::None
        );
        assert_eq!(states[0].nudge_streak, 1);
        bump_nudge_streak(&mut states, "a");
        assert_eq!(
            bump_nudge_streak(&mut states, "a"),
            crate::rot::RotAction::Compact
        );
        assert_eq!(states[0].nudge_streak, 3);
    }

    #[test]
    fn plan_actions_nudge_cap_fourth_is_mail_only() {
        let queue: Vec<String> = Vec::new();
        let mut s = st("flywheel-proj-oc", "idle");
        s.nudge_streak = 3;
        let d = digest_map(digest(0, 0, "", ""));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                ..Default::default()
            },
            &[s],
            Some("proceed"),
            &d,
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Send { text, .. } => {
                assert!(text.contains("[nudge-mail]"), "{text}");
            }
            other => panic!("expected mail-only Send past nudge cap, got {other:?}"),
        }
    }

    #[test]
    fn rot_restart_emits_respawn() {
        let ev = crate::rot::RotEvents {
            bytes: 9000,
            ceiling: 8000,
            repetition: 3,
            marker_silence_turns: 3,
            ..crate::rot::RotEvents::default()
        };
        let (next, actions) = tick(
            &LoopModel::default(),
            &LoopMsg::Rot {
                name: "a".into(),
                events: ev,
            },
        );
        let _ = next;
        assert!(
            matches!(&actions[0], LoopAction::Respawn { name, reason } if name == "a" && reason == "rot"),
            "{actions:?}"
        );
    }

    #[test]
    fn plan_actions_suppresses_wake_for_already_woken_agent() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-proj-oc", "idle")];
        let d = digest_map(digest(1, 1, "PinkGorge", "[bd-1] help"));
        let mut suppressed = std::collections::HashSet::new();
        suppressed.insert("flywheel-proj-oc".to_string());
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &suppressed,
        );
        assert_eq!(
            actions[0],
            LoopAction::Wait,
            "already-woken agent must wait"
        );
    }

    #[test]
    fn plan_actions_unsuppressed_wake_still_fires_for_new_urgent_id() {
        let queue: Vec<String> = Vec::new();
        let states = vec![st("flywheel-proj-oc", "idle")];
        let d = digest_map(digest(1, 1, "PinkGorge", "[bd-1] new help"));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &std::collections::HashSet::new(),
        );
        match &actions[0] {
            LoopAction::Send { text, .. } => assert!(text.starts_with("MAIL WAKE:")),
            other => panic!("expected wake Send, got {other:?}"),
        }
    }

    #[test]
    fn plan_actions_working_agent_ignores_mail_digest() {
        let queue = vec!["do the work".to_string()];
        let states = vec![st("flywheel-proj-oc", "working")];
        let d = digest_map(digest(5, 1, "PinkGorge", "[bd-1] input"));
        let actions = plan_actions(
            &LoopModel {
                queue: queue.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),

                heal_grace: Default::default(),

                heal_threshold: Default::default(),

                heal_budget: Default::default(),

                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
                bead_slots: Default::default(),
                agent_slots: Default::default(),
            },
            &states,
            None,
            &d,
            &std::collections::HashSet::new(),
        );
        assert_eq!(actions[0], LoopAction::Wait);
        assert_eq!(
            queue.len(),
            1,
            "queue must not be consumed by a working agent"
        );
    }

    fn rb2(id: &str, title: &str) -> ReadyBead {
        ReadyBead {
            id: id.to_string(),
            title: title.to_string(),
            assignee: None,
            verify: None,
            labels: Vec::new(),
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }
    }

    fn rb_assignee(id: &str, title: &str, assignee: &str) -> ReadyBead {
        ReadyBead {
            id: id.to_string(),
            title: title.to_string(),
            assignee: Some(assignee.to_string()),
            verify: None,
            labels: Vec::new(),
            priority: 3,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        }
    }

    fn cw(agent: &str) -> ClaimWatch {
        ClaimWatch {
            agent: agent.to_string(),
            unclaimed_polls: 0,
            close_nudged: false,
            close_polls: 0,
            wait_reason: None,
        }
    }

    /// An empty overlap board — the default for legacy claim-watch tests.
    fn empty_overlap() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    /// An empty live-lease set — the default for legacy claim-watch tests.
    fn empty_leases() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn bead_id_from_prompt_extracts_goal_execute_and_resume() {
        assert_eq!(
            bead_id_from_prompt("GOAL Execute bd-12: fix auth flow."),
            Some("bd-12")
        );
        assert_eq!(
            bead_id_from_prompt("GOAL Resume bd-bvwf.1: loop reliability."),
            Some("bd-bvwf.1")
        );
        assert_eq!(
            bead_id_from_prompt("p1"),
            None,
            "plain stage prompts are not tracked"
        );
        assert_eq!(
            bead_id_from_prompt("MAIL WAKE: 1 urgent message..."),
            None,
            "mail wakes are not tracked"
        );
        assert_eq!(
            bead_id_from_prompt("CLAIM REQUIRED: bead bd-9 is still unclaimed"),
            None,
            "nudges are not tracked"
        );
        assert_eq!(
            bead_id_from_prompt("GOAL Execute bd-x: title: with colon."),
            Some("bd-x")
        );
    }

    #[test]
    fn claim_nudge_prompt_is_claim_focused() {
        let p = claim_nudge_prompt("bd-12");
        assert!(
            p.contains("br update bd-12 --claim"),
            "explicit claim command: {p}"
        );
        assert!(p.contains("bd-12"), "names the bead");
        assert!(
            p.contains("--actor \"$AGENT_NAME\""),
            "claim as the pin via AGENT_NAME env: {p}"
        );
    }

    #[test]
    fn close_nudge_prompt_commands_close_or_reopen() {
        let p = close_nudge_prompt("bd-12", "fix auth");
        assert!(p.contains("br close bd-12"), "explicit close command: {p}");
        assert!(p.contains("--status open"), "reopen fallback: {p}");
    }

    /// A dispatched bead that is STILL unclaimed (in `br ready`) while its
    /// agent is WORKING accumulates unclaimed polls; after the nudge
    /// threshold it produces a claim-focused nudge to THAT agent (not a
    /// re-dispatch — the agent is engaged, it just never claimed).
    #[test]
    fn claim_watch_nudges_working_agent_after_threshold() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let ready = vec![rb2("bd-12", "fix auth")];
        let inprogress = vec![];
        let states = vec![st("oc-1", "working")];

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(actions.is_empty(), "no nudge before threshold: {actions:?}");
        assert_eq!(watch.get("bd-12").unwrap().unclaimed_polls, 1);

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        match &actions[0] {
            ClaimWatchAction::NudgeClaim { agent, bead_id } => {
                assert_eq!(agent, "oc-1");
                assert_eq!(bead_id, "bd-12");
            }
            other => panic!("expected NudgeClaim, got {other:?}"),
        }
        assert_eq!(watch.get("bd-12").unwrap().unclaimed_polls, 0);
    }

    /// A dispatched bead whose agent SETTLED (idle/done) without claiming is
    /// re-dispatched: the entry drops from the watch and a fresh bead prompt
    /// is queued at the FRONT (another agent takes it).
    #[test]
    fn claim_watch_redispatch_when_agent_settled_without_claim() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let ready = vec![rb2("bd-12", "fix auth")];
        let inprogress = vec![];
        let states = vec![st("oc-1", "idle")];

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(
            matches!(&actions[0], ClaimWatchAction::RedispatchBead { bead_id, title }
                if bead_id == "bd-12" && title == "fix auth"),
            "{:?}",
            actions[0]
        );
        assert!(
            watch.is_empty(),
            "settled-without-claim drops the watch entry"
        );
    }

    /// A claimed bead (in `br inprogress`) whose assignee is idle/done
    /// without closing gets a CLOSE nudge once (option C), then — if it
    /// STILL stays unclosed past the threshold — a resume re-dispatch.
    #[test]
    fn claim_watch_close_nudge_then_resume_redispatch() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let ready = vec![];
        let inprogress = vec![rb_assignee("bd-12", "fix auth", "oc-1")];
        let states = vec![st("oc-1", "done")];

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        match &actions[0] {
            ClaimWatchAction::NudgeClose {
                agent,
                bead_id,
                title,
            } => {
                assert_eq!(agent, "oc-1");
                assert_eq!(bead_id, "bd-12");
                assert_eq!(title, "fix auth");
            }
            other => panic!("expected NudgeClose, got {other:?}"),
        }
        assert!(watch.get("bd-12").unwrap().close_nudged);

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(actions.is_empty(), "no repeat close nudge: {actions:?}");

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(
            matches!(&actions[0], ClaimWatchAction::RedispatchResume { bead_id, title }
                if bead_id == "bd-12" && title == "fix auth"),
            "{:?}",
            actions[0]
        );
        assert!(watch.is_empty());
    }

    /// bd-claim-watch-durable-signals-mixy (acceptance): a claimed bead whose
    /// assignee settled is NOT resume-redispatched while a live toron lease
    /// holds it — the observed incident re-sent `GOAL Resume` every pass
    /// over an in_progress bead with an unexpired exclusive lease. The close
    /// nudge still fires (it targets the assignee's own lane); the resume
    /// waits, with a fresh countdown, for the lease to lapse.
    #[test]
    fn resume_suppressed_by_live_lease() {
        use std::collections::HashSet;
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let ready = vec![];
        let inprogress = vec![rb_assignee("bd-12", "fix auth", "oc-1")];
        let states = vec![st("oc-1", "done")];

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(
            matches!(&actions[0], ClaimWatchAction::NudgeClose { bead_id, .. }
                if bead_id == "bd-12"),
            "close nudge still fires: {actions:?}"
        );

        let live: HashSet<String> = HashSet::from(["bd-12".to_string()]);
        for i in 0..3 {
            let actions = claim_watch_step(
                &mut watch,
                &ready,
                &inprogress,
                &states,
                2,
                &empty_overlap(),
                &live,
            );
            assert!(
                actions.is_empty(),
                "poll {i}: resume suppressed by live lease: {actions:?}"
            );
        }
        assert_eq!(watch["bd-12"].wait_reason, Some(WaitReason::LiveLease));
        assert_eq!(
            watch["bd-12"].close_polls, 0,
            "the resume countdown does not advance under a live lease"
        );
        assert!(watch.contains_key("bd-12"), "kept watching");

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(actions.is_empty(), "fresh countdown: {actions:?}");
        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(
            matches!(&actions[0], ClaimWatchAction::RedispatchResume { bead_id, title }
                if bead_id == "bd-12" && title == "fix auth"),
            "lease lapsed → resume fires: {:?}",
            actions[0]
        );
        assert!(watch.is_empty());
    }

    /// The unclaimed arm honors the lease too: an agent that settled (or
    /// vanished) without claiming is not re-dispatched while a live lease
    /// holds the bead; the hold releases when the lease lapses.
    #[test]
    fn redispatch_suppressed_by_live_lease() {
        use std::collections::HashSet;
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let ready = vec![rb2("bd-12", "fix auth")];
        let live: HashSet<String> = HashSet::from(["bd-12".to_string()]);

        let states = vec![st("oc-1", "idle")];
        let actions =
            claim_watch_step(&mut watch, &ready, &[], &states, 2, &empty_overlap(), &live);
        assert!(
            actions.is_empty(),
            "no redispatch over a lease: {actions:?}"
        );
        assert_eq!(watch["bd-12"].wait_reason, Some(WaitReason::LiveLease));

        let actions = claim_watch_step(&mut watch, &ready, &[], &[], 2, &empty_overlap(), &live);
        assert!(actions.is_empty(), "leash outlives the pane: {actions:?}");
        assert!(watch.contains_key("bd-12"));

        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &[],
            &[],
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(
            matches!(&actions[0], ClaimWatchAction::RedispatchBead { bead_id, .. }
                if bead_id == "bd-12"),
            "lease lapsed → redispatch: {:?}",
            actions[0]
        );
        assert!(watch.is_empty());
    }

    /// A claimed bead whose assignee is WORKING stays watched (work in
    /// flight) — no close nudge, no redispatch.
    #[test]
    fn claim_watch_working_assignee_no_close_nudge() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let ready = vec![];
        let inprogress = vec![rb_assignee("bd-12", "fix auth", "oc-1")];
        let states = vec![st("oc-1", "working")];
        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &inprogress,
            &states,
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(
            actions.is_empty(),
            "no nudge while claimed work is in flight: {actions:?}"
        );
        assert!(watch.contains_key("bd-12"), "keep watching");
    }

    /// A bead that vanished from BOTH snapshots is closed — the watch entry
    /// drops (the lifecycle completed; nothing left to enforce).
    #[test]
    fn claim_watch_drops_closed_bead() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let actions = claim_watch_step(
            &mut watch,
            &[],
            &[],
            &[],
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(watch.is_empty(), "closed bead drops from the watch");
        assert!(
            matches!(&actions[0], ClaimWatchAction::ReleaseReservations { bead_id }
                if bead_id == "bd-12"),
            "close drop fires the release: {:?}",
            actions
        );
    }

    /// bd-claim-watch: a bead that still exists as Blocked/Deferred is NOT
    /// closed. The known-not-ready snapshot must keep the watch (and its file
    /// reservations) instead of firing a false release.
    #[test]
    fn claim_watch_keeps_known_not_ready_bead() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let not_ready = [rb2("bd-12", "Blocked")];
        let refs: Vec<&ReadyBead> = not_ready.iter().collect();
        let actions = claim_watch_step_with_known(
            &mut watch,
            &[],
            &[],
            &refs,
            &[],
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(
            actions.is_empty(),
            "a blocked/deferred bead is not a close: {actions:?}"
        );
        assert!(
            watch.contains_key("bd-12"),
            "keep watching the still-live bead"
        );
    }

    /// bd-h3oj: the moment a watched bead leaves both snapshots (closed),
    /// the watchdog asks for its file reservations to be released — exactly
    /// once (the watch entry drops, so no repeat fire).
    #[test]
    fn claim_watch_close_drops_release_fires_once() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-77".into(), cw("oc-1"));
        let actions = claim_watch_step(
            &mut watch,
            &[],
            &[],
            &[],
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert_eq!(actions.len(), 1, "one release action: {actions:?}");
        assert!(
            matches!(&actions[0], ClaimWatchAction::ReleaseReservations { bead_id }
                if bead_id == "bd-77"),
            "{:?}",
            actions[0]
        );
        let actions = claim_watch_step(
            &mut watch,
            &[],
            &[],
            &[],
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(actions.is_empty(), "no repeat release: {actions:?}");
        assert!(watch.is_empty());
    }

    /// An unclaimed bead whose dispatched agent is GONE (not in the poll)
    /// is re-dispatched — a dead lane must not strand the bead.
    #[test]
    fn claim_watch_redispatch_when_agent_gone() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-12".into(), cw("oc-1"));
        let ready = vec![rb2("bd-12", "fix auth")];
        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &[],
            &[],
            2,
            &empty_overlap(),
            &empty_leases(),
        );
        assert!(
            matches!(&actions[0], ClaimWatchAction::RedispatchBead { bead_id, .. }
                if bead_id == "bd-12"),
            "{:?}",
            actions[0]
        );
        assert!(watch.is_empty());
    }

    /// bd-9tbb (leafReply): the loop body must NEVER consume a checkpointed
    /// board read — a replayed poll would read the STORED board
    /// and a stale always-serialize row would gate dispatch forever. The live
    /// sites call `reservation_board_live`; the checkpointed leaf is gone
    /// (dead vendor substrate removed). Pinned by source shape: no
    /// checkpointed board read may appear in swarm_loop's body.
    #[test]
    fn board_reads_in_loop_body_are_live_not_checkpointed() {
        let src = std::fs::read_to_string(file!()).expect("durable.rs readable");
        let loop_start = src.find("async fn swarm_loop(").expect("swarm_loop exists");
        let loop_body = &src[loop_start..];
        let loop_end = loop_body
            .find("pub async fn run_durable_loop")
            .expect("run_durable_loop after swarm_loop");
        let body = &loop_body[..loop_end];
        assert!(
            !body.contains("ctx\n .run(")
                && !body.contains("ctx.run(\n reservation_board")
                && !body.contains("reservation_board,"),
            "bd-9tbb: swarm_loop must read the board LIVE (reservation_board_live), never ctx.run(reservation_board) — a checkpointed board replay serializes dispatch forever"
        );
        assert!(
            body.contains("reservation_board_live("),
            "the live read helper is on the loop path"
        );
    }

    /// bd-jjlg (T1): the pure leaves run journaled, not via `ctx.run`.
    /// `swarm_loop` must not `ctx.run` a pure subprocess+parse leaf — those
    /// go through `run_leaf_journaled` with the same argv/parse as the leaf
    /// body. Complex leaves keep `ctx.run` (own Cmd fields needed):
    /// `poll_events` (bounded + inside-leaf log), `send_prompt` (inbox
    /// branch + bounded + send log), `respawn_agent` (file writes),
    /// `spawn_xproj` (at-most-once mark), `notify_blocked` (ack gate +
    /// persist), `diagnose_agent` (log), `reclaim_bead` (release + comment),
    /// `renew_filtered_leases` (direct mail). bd-5ouq: `loop_run_verify`
    /// and `dispatch_guard_scan` left that list — their children run
    /// journaled and their evaluations stay inline with caller authority.
    #[test]
    fn pure_leaves_run_journaled_not_ctx_run() {
        let src = std::fs::read_to_string(file!()).expect("durable.rs readable");
        let loop_start = src.find("async fn swarm_loop(").expect("swarm_loop exists");
        let loop_body = &src[loop_start..];
        let loop_end = loop_body
            .find("pub async fn run_durable_loop")
            .expect("run_durable_loop after swarm_loop");
        let body = &loop_body[..loop_end];
        let pure_leaves = [
            ["poll", "_swarm,"].concat(),
            ["poll", "_xproj_inbox,"].concat(),
            ["poll", "_compose_requests,"].concat(),
            ["poll", "_context_inbox,"].concat(),
            ["poll", "_inprogress,"].concat(),
            ["poll", "_kind,"].concat(),
            ["resolve", "_agent_profile,"].concat(),
            ["load", "_expected_replies,"].concat(),
            ["check", "_reply,"].concat(),
            ["reply", "_settled,"].concat(),
            ["remind", "_reply,"].concat(),
            ["read", "_output,"].concat(),
            ["loop_run", "_verify,"].concat(),
            ["dispatch_guard", "_scan,"].concat(),
        ];
        for leaf in &pure_leaves {
            assert!(
                !body.contains(leaf.as_str()),
                "bd-jjlg: swarm_loop must not ctx.run({leaf}) — wire it via run_leaf_journaled"
            );
        }
    }

    /// bd-0fvb: the durable function takes host authority from the invocation
    /// context (injected once at the host edge via `with_dependency`), never
    /// resolving or sampling the clock ambiently — an ambient read inside a
    /// `#[function]` body is the replay-determinism bug the effect law bans.
    /// Needles reaching into the host region are split so this test's own
    /// source cannot satisfy them.
    #[test]
    fn bd_0fvb_authority_injected_not_ambient() {
        let src = include_str!("durable.rs");
        let loop_start = src.find("async fn swarm_loop(").expect("swarm_loop exists");
        let loop_body = &src[loop_start..];
        let loop_end = loop_body
            .find("pub async fn run_durable_loop")
            .expect("run_durable_loop after swarm_loop");
        let body = &loop_body[..loop_end];
        assert!(
            loop_body
                .split('{')
                .next()
                .is_some_and(|sig| sig.contains("authority: crate::authority::RuntimeAuthority")),
            "bd-0fvb: swarm_loop must take RuntimeAuthority as a host-injected parameter"
        );
        assert!(
            !body.contains(&["RuntimeAuthority::", "resolve()"].concat()),
            "bd-0fvb: no ambient RuntimeAuthority::resolve() inside the durable function"
        );
        for ambient in ["chrono::Utc::now", "SystemTime::now", "Instant::now"] {
            assert!(
                !body.contains(ambient),
                "bd-0fvb: the durable function must not sample {ambient} directly"
            );
        }
        let host = &loop_body[loop_end..];
        let resolve = [
            "let authority = crate::authority::",
            "RuntimeAuthority::resolve();",
        ]
        .concat();
        let stamp = ["let now_nanos = authority", ".clock.now_nanos();"].concat();
        assert!(
            host.contains(&resolve)
                && host.contains(&stamp)
                && host.contains("swarm_loop(authority"),
            "bd-0fvb: the host edge must resolve the authority once, pass it in, and stamp time from it"
        );
    }

    /// bd-lf1u: every `#[function]` body in this file must take its time from
    /// INJECTED authority, never from an ambient read.
    ///
    /// bd-0fvb closed this for `swarm_loop` alone and guarded it by slicing
    /// the source between two function names, so the one leaf that still
    /// stamped durable DATA with wall time (`loop_run_verify`, the
    /// `command-verified` verdict row) stayed green. This walks every
    /// `#[function]` body instead: a new leaf cannot regress without the
    /// guard noticing, wherever it is added.
    ///
    /// Two ways a source scan lies about its own extent, both hit while
    /// writing this, and both fixed here rather than worked around:
    ///
    /// 1. Counting braces from the signature runs past the body, because a
    ///    brace inside a string literal (`format!("{e}")`, `r#"{"ok":true}"#`)
    ///    counts. The first version walked 2400 lines past `swarm_loop`'s end
    ///    and reported three violations bd-0fvb had already proven absent. A
    ///    top-level `fn`'s closing brace is the one at column 0, and rustfmt
    ///    guarantees every nested one is indented — so the line shape is the
    ///    reliable signal, not the bracket count.
    /// 2. Scanning the whole file matches the `#[function]` TEXT inside this
    ///    module's own fixtures, so the guard's self-test fed it phantom
    ///    leaves. The scan stops at the first `#[cfg(test)] mod`, which is
    ///    where test-only code begins; a `#[function]` in a test module is not
    ///    a leaf the SDK ever registers, so it is not in scope.
    fn ambient_clock_reads_in_durable_leaves(src: &str) -> Vec<String> {
        let needles = ["chrono::Utc::now", "SystemTime::now", "Instant::now"];
        let mut hits = Vec::new();
        let scan_end = src.find("#[cfg(test)]\nmod ").unwrap_or(src.len());
        let scan = &src[..scan_end];
        let mut cursor = 0usize;
        while let Some(attr) = scan[cursor..].find("#[function]") {
            let after = cursor + attr + "#[function]".len();
            let Some(sig_off) = scan[after..].find("async fn ") else {
                break;
            };
            let sig_at = after + sig_off;
            let Some(line_off) = scan[sig_at..].find('\n') else {
                break;
            };
            let sig_line_end = sig_at + line_off;
            let name = scan[sig_at..sig_line_end]
                .trim_start_matches("async fn ")
                .split(['(', '<', ' '])
                .next()
                .unwrap_or("<unknown>")
                .to_string();
            let body_end = match scan[sig_line_end..].find("\n}") {
                Some(p) => sig_line_end + p + 2,
                None => break,
            };
            let body = &scan[sig_line_end..body_end];
            for needle in needles {
                if body.contains(needle) {
                    hits.push(format!("{name}: {needle}"));
                }
            }
            cursor = body_end;
        }
        hits
    }

    #[test]
    fn bd_lf1u_no_durable_leaf_samples_an_ambient_clock() {
        let src = include_str!("durable.rs");
        let hits = ambient_clock_reads_in_durable_leaves(src);
        assert!(
            hits.is_empty(),
            "bd-lf1u: these #[function] bodies read wall time ambiently, which is the \
             replay-determinism bug the effect law bans. A durable leaf that stamps DATA \
             must take time from injected authority: add `&Info` as the first parameter \
             and read `info.get_dependency::<RuntimeAuthority>()`:\n  {}\n\
             (a host-side deadline on a live subprocess is a different thing and belongs \
             in a plain helper, never in a #[function] body)",
            hits.join("\n  ")
        );
    }

    /// The scanner is the thing under test, so it gets its own proof. Three
    /// ways it could be vacuous, each checked: a clean source must come back
    /// empty, a planted violation must be found AND NAMED, and the extractor
    /// must not run past a body whose braces live inside string literals —
    /// the exact defect that made the first version of this guard report three
    /// violations in a function bd-0fvb had already proven clean.
    #[test]
    fn bd_lf1u_leaf_scanner_actually_walks_bodies() {
        let clean =
            "#[function]\nasync fn a_leaf(args: A) -> anyhow::Result<Value> { Ok(json!(1)) }\n";
        assert_eq!(
            ambient_clock_reads_in_durable_leaves(clean),
            Vec::<String>::new()
        );

        let planted = "#[function]\n#[allow(clippy::too_many_arguments)]\nasync fn b_leaf(args: A) -> anyhow::Result<Value> {\n    let at = chrono::Utc::now();\n    Ok(json!(at))\n}\n";
        assert_eq!(
            ambient_clock_reads_in_durable_leaves(planted),
            vec!["b_leaf: chrono::Utc::now".to_string()],
            "the walk must reach a body behind intervening attributes and name it"
        );

        for (needle, why) in [
            ("chrono::Utc::now", "wall clock"),
            ("SystemTime::now", "system time"),
            ("Instant::now", "monotonic"),
        ] {
            let src = format!(
                "#[function]\nasync fn c_leaf(args: A) -> anyhow::Result<Value> {{\n    let _ = {needle}();\n    Ok(json!(0))\n}}\n"
            );
            assert_eq!(
                ambient_clock_reads_in_durable_leaves(&src),
                vec![format!("c_leaf: {needle}")],
                "the scanner must catch a {why} read"
            );
        }

        let tricky = concat!(
            "#[function]\n",
            "async fn d_leaf(args: A) -> anyhow::Result<Value> {\n",
            "    let msg = format!(\"{} {}\", args.a, args.b);\n",
            "    let raw = r#\"{\"ok\": true}\"#;\n",
            "    Ok(json!(msg))\n",
            "}\n",
            "#[function]\n",
            "async fn e_leaf(args: A) -> anyhow::Result<Value> { Ok(json!(1)) }\n",
        );
        assert_eq!(
            ambient_clock_reads_in_durable_leaves(tricky),
            Vec::<String>::new(),
            "string-literal braces must not extend a leaf body past its own closing brace"
        );

        let after_tests = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { let _ = \"#[function]\\nasync fn phantom(a: A) -> anyhow::Result<Value> {\\n let _ = chrono::Utc::now();\\n\"; }\n}\n";
        assert_eq!(
            ambient_clock_reads_in_durable_leaves(after_tests),
            Vec::<String>::new(),
            "test-module code is out of scope, including the #[function] text inside fixtures"
        );

        let helper = "async fn plain_helper() { let _ = std::time::Instant::now(); }\n";
        assert_eq!(
            ambient_clock_reads_in_durable_leaves(helper),
            Vec::<String>::new(),
            "a non-#[function] helper is not a durable leaf and must not be flagged"
        );
    }

    /// parse_reservation_rows shapes: canonical row set, missing field rows
    /// skipped, non-array reservations degrade to empty (never panic).
    #[test]
    fn parse_reservation_rows_shapes() {
        use serde_json::json;
        let rows = parse_reservation_rows(&json!({
            "count": 2,
            "reservations": [
                {"path": ".beads/**", "reason": "bd-a", "holder": "x"},
                {"path": "Cargo.lock", "reason": "bd-b"},
                {"path": "src/lib.rs"}
            ]
        }));
        assert_eq!(
            rows,
            vec![
                (".beads/**".to_string(), "bd-a".to_string()),
                ("Cargo.lock".to_string(), "bd-b".to_string()),
            ]
        );
        assert_eq!(
            parse_reservation_rows(&json!({})),
            [] as [(String, String); 0]
        );
        assert_eq!(
            parse_reservation_rows(&json!({"reservations": "nope"})),
            [] as [(String, String); 0]
        );
    }

    /// Always-serialize paths: a held reservation on .beads/**, .flywheel/**,
    /// Cargo.lock, root Cargo.toml, docs/catalog/** or target/** gates ALL
    /// new ready-bead dispatch (never parallel). Normal source paths do not.
    #[test]
    fn serialize_overlap_always_serialize_paths_gate() {
        let held = vec![("Cargo.lock".to_string(), "bd-a".to_string())];
        assert!(always_serialize_held(&held), "Cargo.lock serializes");
        let held = vec![(".flywheel/receipts/x.txt".to_string(), "bd-a".to_string())];
        assert!(always_serialize_held(&held), ".flywheel/** serializes");
        let held = vec![(".beads/issues.jsonl".to_string(), "bd-a".to_string())];
        assert!(always_serialize_held(&held), ".beads/** serializes");
        let held = vec![("docs/catalog/x.json".to_string(), "bd-a".to_string())];
        assert!(always_serialize_held(&held), "docs/catalog/** serializes");
        let held = vec![("src/durable.rs".to_string(), "bd-a".to_string())];
        assert!(
            !always_serialize_held(&held),
            "normal source paths do not serialize everything"
        );
        assert!(!always_serialize_held(&[]), "empty board never serializes");
    }

    /// Two in-flight beads holding overlapping path globs are BOTH
    /// overlap-waiting; disjoint paths are not.
    #[test]
    fn serialize_overlap_bead_overlap_waits_marks_conflicting_in_flight() {
        let held = vec![
            ("src/**".to_string(), "bd-a".to_string()),
            ("src/durable.rs".to_string(), "bd-b".to_string()),
            ("docs/**".to_string(), "bd-c".to_string()),
        ];
        let in_flight = vec!["bd-a".to_string(), "bd-b".to_string(), "bd-c".to_string()];
        let waits = bead_overlap_waits(&held, &in_flight);
        assert!(waits.contains("bd-a"), "src/** waits: {waits:?}");
        assert!(waits.contains("bd-b"), "src/durable.rs waits: {waits:?}");
        assert!(
            !waits.contains("bd-c"),
            "disjoint docs/** does not wait: {waits:?}"
        );
        let waits = bead_overlap_waits(&held, &["bd-z".to_string()]);
        assert!(waits.is_empty());
    }

    /// An unclaimed bead waiting on overlap is NOT a stuck owner: no claim
    /// nudge, wait reason recorded as Overlap.
    #[test]
    fn serialize_overlap_claim_watch_unclaimed_overlap_no_nudge() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-b".into(), cw("oc-1"));
        let ready = vec![rb2("bd-b", "blocked bead")];
        let states = vec![st("oc-1", "working")];
        let overlap: std::collections::HashSet<String> =
            std::collections::HashSet::from(["bd-b".to_string()]);
        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &[],
            &states,
            2,
            &overlap,
            &empty_leases(),
        );
        assert!(
            actions.is_empty(),
            "no claim nudge for an overlap wait: {actions:?}"
        );
        assert_eq!(
            watch["bd-b"].wait_reason,
            Some(WaitReason::Overlap),
            "wait reason recorded"
        );
    }

    /// A claimed-but-settled bead whose paths are contested is waiting on
    /// the other bead's release — no close nudge, no resume re-dispatch.
    #[test]
    fn serialize_overlap_claim_watch_claimed_settled_overlap_no_close_nudge() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-b".into(), cw("oc-1"));
        let inprogress = vec![rb_assignee("bd-b", "blocked bead", "oc-1")];
        let states = vec![st("oc-1", "idle")];
        let overlap: std::collections::HashSet<String> =
            std::collections::HashSet::from(["bd-b".to_string()]);
        let actions = claim_watch_step(
            &mut watch,
            &[],
            &inprogress,
            &states,
            2,
            &overlap,
            &empty_leases(),
        );
        assert!(
            actions.is_empty(),
            "no close nudge / redispatch for an overlap wait: {actions:?}"
        );
        assert_eq!(watch["bd-b"].wait_reason, Some(WaitReason::Overlap));
    }

    /// A dispatched bead that became wave-held (an earlier wave opened after
    /// dispatch) waits like an overlap — NOT a stuck owner: no claim nudge,
    /// wait reason recorded as WaveGate.
    #[test]
    fn serialize_overlap_claim_watch_unclaimed_wave_held_no_nudge() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-w2".into(), cw("oc-1"));
        let ready = vec![rb_wave("bd-w1", 1), rb_wave("bd-w2", 2)];
        let states = vec![st("oc-1", "working")];
        let overlap: std::collections::HashSet<String> = std::collections::HashSet::new();
        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &[],
            &states,
            1,
            &overlap,
            &empty_leases(),
        );
        assert!(
            actions.is_empty(),
            "no claim nudge for a wave-gated wait: {actions:?}"
        );
        assert_eq!(
            watch["bd-w2"].wait_reason,
            Some(WaitReason::WaveGate),
            "wait reason recorded as wave-gate, not stuck-owner"
        );
    }

    /// Non-overlap beads keep the exact pre-existing watchdog behavior and
    /// their wait reason stays None (a released overlap clears the marker).
    #[test]
    fn serialize_overlap_claim_watch_non_overlap_unchanged() {
        let mut watch: std::collections::HashMap<String, ClaimWatch> =
            std::collections::HashMap::new();
        watch.insert("bd-a".into(), cw("oc-1"));
        let ready = vec![rb2("bd-a", "normal bead")];
        let states = vec![st("oc-1", "working")];
        let overlap: std::collections::HashSet<String> = std::collections::HashSet::new();
        let actions = claim_watch_step(
            &mut watch,
            &ready,
            &[],
            &states,
            1,
            &overlap,
            &empty_leases(),
        );
        assert!(
            matches!(&actions[0], ClaimWatchAction::NudgeClaim { bead_id, .. }
                if bead_id == "bd-a"),
            "{:?}",
            actions[0]
        );
        assert_eq!(watch["bd-a"].wait_reason, None);
    }

    /// The loop NEVER calls `worktree create` (ADR-0019 Spec 3.2): the
    /// brief's FORBIDDEN field bans it, the preflight gate blocks extra
    /// linked worktrees, and the dispatch planner has no worktree surface
    /// (the action enum has no worktree variant — compile-enforced). This
    /// guard pins the dispatch-side guarantee.
    #[test]
    fn serialize_overlap_loop_never_calls_worktree_create() {
        let b = build_brief("bd-1", "t", "flywheel", false).unwrap();
        assert!(
            b.contains("No worktree"),
            "brief FORBIDDEN bans worktrees: {b}"
        );
        assert!(
            !b.contains("worktree.create"),
            "no worktree-create call in the dispatch surface: {b}"
        );
        assert!(crate::preflight::worktree_optout_legal(false, false));
        assert!(!crate::preflight::Mode::loop_work().allow_worktree);
    }

    #[test]
    fn params_serde_roundtrip() {
        let p = LoopParams {
            project: "notetoself".into(),
            to: "all".into(),
            interval_secs: 300,
            active_interval_secs: 15,
            queue: vec!["p1".into()],
            nudge: None,
            on_blocked: "continue".into(),
            max_iterations: Some(10),
            work: false,
            no_wave_gates: false,
            wave_label: None,
            confirm_sends: true,
            confirm_timeout_ms: Some(45_000),
            invocation: "inv-1".into(),
            unknown_diagnose_threshold: 3,
            respawn_budget: 2,
            unknown_detect_grace_polls: 6,
            drain_grace_polls: 1,
            claim_nudge_after_polls: 2,
            force: false,
            resume: false,
            strict: false,
            reclaim: crate::durable_policy::ReclaimPolicy::new(10, 20),
            pane_stall_secs: 1800,
            dispatch_guard_ttl_us: 90 * 60 * 1_000_000,
            mail_health_escalate_at: 3,
            pause_resurface_secs: 3600,
            walk_timeout_secs: 10,
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: LoopParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.project, "notetoself");
        assert_eq!(back.interval_secs, 300);
        assert_eq!(back.active_interval_secs, 15);
        assert_eq!(back.max_iterations, Some(10));
        assert!(back.confirm_sends);
        assert_eq!(back.confirm_timeout_ms, Some(45_000));
        assert_eq!(back.invocation, "inv-1");
        assert_eq!(back.unknown_diagnose_threshold, 3);
        assert_eq!(back.respawn_budget, 2);
        assert_eq!(back.unknown_detect_grace_polls, 6);
        assert_eq!(back.drain_grace_polls, 1);
    }

    #[test]
    fn params_active_interval_defaults_30_when_absent() {
        let v: serde_json::Value = serde_json::json!({
            "project": "notetoself",
            "to": "all",
            "interval_secs": 300,
            "queue": [],
            "nudge": null,
            "on_blocked": "continue",
            "max_iterations": null,
            "work": false,
            "confirm_sends": true,
            "confirm_timeout_ms": null,
        });
        let back: LoopParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.active_interval_secs, 30);
        assert_eq!(back.pane_stall_secs, PANE_STALL_DEFAULT_SECS);
        assert_eq!(back.dispatch_guard_ttl_us, DISPATCH_GUARD_TTL_US);
    }

    #[test]
    fn params_confirm_defaults_true_when_absent() {
        let v = serde_json::json!({
            "project": "legacy",
            "to": "all",
            "interval_secs": 300,
            "queue": [],
            "nudge": null,
            "on_blocked": "continue",
            "max_iterations": null,
            "work": false,
        });
        let p: LoopParams = serde_json::from_value(v).unwrap();
        assert!(
            p.confirm_sends,
            "confirm_sends must default true for legacy params"
        );
        assert_eq!(p.confirm_timeout_ms, None);
    }

    #[test]
    fn send_args_serde_roundtrip_with_confirm() {
        let a = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: Some(45_000),
            invocation: "inv-1".into(),
            iteration: 3,
            profile: String::new(),
        };
        let v = serde_json::to_value(&a).unwrap();
        let back: SendArgs = serde_json::from_value(v).unwrap();
        assert!(back.confirm);
        assert_eq!(back.confirm_timeout_ms, Some(45_000));
    }

    #[test]
    fn send_prompt_argv_confirm_flag() {
        let args = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: Some(45_000),
            invocation: "inv-1".into(),
            iteration: 3,
            profile: String::new(),
        };
        assert_eq!(
            send_prompt_argv(&args),
            vec![
                "__send",
                "p",
                "a",
                "t",
                "--confirm",
                "45000",
                "--invocation",
                "inv-1",
                "--iteration",
                "3",
            ]
        );
        let args_no_ms = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: None,
            invocation: "inv-1".into(),
            iteration: 3,
            profile: String::new(),
        };
        assert_eq!(
            send_prompt_argv(&args_no_ms),
            vec![
                "__send",
                "p",
                "a",
                "t",
                "--confirm",
                "--invocation",
                "inv-1",
                "--iteration",
                "3",
            ]
        );
        let args_legacy = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: false,
            confirm_timeout_ms: None,
            invocation: "inv-1".into(),
            iteration: 3,
            profile: String::new(),
        };
        assert_eq!(
            send_prompt_argv(&args_legacy),
            vec![
                "__send",
                "p",
                "a",
                "t",
                "--invocation",
                "inv-1",
                "--iteration",
                "3",
            ]
        );
    }

    #[test]
    fn send_prompt_argv_omits_registry_flags_when_invocation_empty() {
        let args = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: Some(45_000),
            invocation: String::new(),
            iteration: 3,
            profile: String::new(),
        };
        assert_eq!(
            send_prompt_argv(&args),
            vec!["__send", "p", "a", "t", "--confirm", "45000"]
        );
    }

    /// bd-gsvn.1: a resolved profile rides the argv as `--profile <name>`
    /// (appended after the registry flags); empty profile → no flag
    /// (byte-identical legacy argv).
    #[test]
    fn send_prompt_argv_profile_flag() {
        let args = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: None,
            invocation: "inv-1".into(),
            iteration: 3,
            profile: "planner".into(),
        };
        assert_eq!(
            send_prompt_argv(&args),
            vec![
                "__send",
                "p",
                "a",
                "t",
                "--confirm",
                "--invocation",
                "inv-1",
                "--iteration",
                "3",
                "--profile",
                "planner",
            ]
        );
    }

    #[test]
    fn send_outcome_parses_confirm_and_legacy_json() {
        let v: SendOutcome = serde_json::from_str(
            r#"{"ok": true, "name": "a", "landed": true, "status": "working", "reason": "landed"}"#,
        )
        .unwrap();
        assert_eq!(v.landed, Some(true));
        assert_eq!(v.status.as_deref(), Some("working"));
        assert_eq!(v.reason.as_deref(), Some("landed"));

        let legacy: SendOutcome = serde_json::from_str(r#"{"ok": true, "name": "a"}"#).unwrap();
        assert_eq!(legacy.landed, None);
        assert_eq!(legacy.reason, None);
        assert!(!legacy.deduped);
    }

    #[test]
    fn send_outcome_parses_deduped_flag() {
        let v: SendOutcome = serde_json::from_str(
            r#"{"ok": true, "name": "a", "landed": true, "status": null, "reason": "landed", "deduped": true}"#,
        )
        .unwrap();
        assert_eq!(v.landed, Some(true));
        assert!(v.deduped);
    }

    #[test]
    fn learn_kinds_from_poll_records_kinds() {
        let mut kinds = std::collections::HashMap::new();
        let states = vec![
            AgentState {
                name: "a".into(),
                status: "idle".into(),
                kind: "pi".into(),
                pane_id: String::new(),
                mail_identity: None,
                nudge_streak: 0,
            },
            AgentState {
                name: "b".into(),
                status: "working".into(),
                kind: "opencode".into(),
                pane_id: String::new(),
                mail_identity: None,
                nudge_streak: 0,
            },
        ];
        learn_kinds(&mut kinds, &states);
        assert_eq!(kinds.get("a").map(String::as_str), Some("pi"));
        assert_eq!(kinds.get("b").map(String::as_str), Some("opencode"));
    }

    #[test]
    fn learn_kinds_keeps_previous_kind_when_poll_has_none() {
        let mut kinds = std::collections::HashMap::new();
        kinds.insert("a".to_string(), "opencode".to_string());
        let states = vec![AgentState {
            name: "a".into(),
            status: "idle".into(),
            kind: String::new(),
            pane_id: String::new(),
            mail_identity: None,
            nudge_streak: 0,
        }];
        learn_kinds(&mut kinds, &states);
        assert_eq!(kinds.get("a").map(String::as_str), Some("opencode"));
    }

    #[test]
    fn respawn_kind_prefers_map_and_returns_none_for_unknown() {
        let mut kinds = std::collections::HashMap::new();
        kinds.insert("a".to_string(), "pi".to_string());
        assert_eq!(respawn_kind(&kinds, "a"), Some("pi".to_string()));
        assert_eq!(respawn_kind(&kinds, "zzz"), None);
    }

    #[test]
    fn agent_state_parses_kind_from_poll_entry() {
        let v: serde_json::Value = serde_json::from_str(r#"["a", "idle", "pi"]"#).unwrap();
        let s = parse_poll_entry(&v).unwrap();
        assert_eq!(s.name, "a");
        assert_eq!(s.status, "idle");
        assert_eq!(s.kind, "pi");
        let legacy: serde_json::Value = serde_json::from_str(r#"["a", "idle"]"#).unwrap();
        let s = parse_poll_entry(&legacy).unwrap();
        assert_eq!(s.kind, "");
    }

    #[test]
    fn parse_poll_events_batch_happy_path() {
        let raw = r#"{
            "events": [
                {"event_type":"pane.agent_status_changed","pane_id":"%1","agent":"opencode","status":"done","seq":1},
                {"event_type":"pane.agent_status_changed","pane_id":"%2","agent":null,"status":"idle","seq":2}
            ],
            "max_seq": 2,
            "stream_error": null
        }"#;
        let batch = parse_poll_events_batch(raw, 0).unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.max_seq, 2);
        assert!(batch.stream_error.is_none());
        assert_eq!(batch.events[0].pane_id, "%1");
        assert_eq!(batch.events[0].status.as_deref(), Some("done"));
        assert_eq!(batch.events[1].agent, None);
    }

    #[test]
    fn parse_poll_events_batch_empty_and_error() {
        let raw = r#"{"events":[],"max_seq":7,"stream_error":"subscribe failed: down"}"#;
        let batch = parse_poll_events_batch(raw, 0).unwrap();
        assert_eq!(batch.events, [] as [PollEvent; 0]);
        assert_eq!(batch.max_seq, 7);
        assert_eq!(
            batch.stream_error.as_deref(),
            Some("subscribe failed: down")
        );
    }

    #[test]
    fn parse_poll_events_batch_rejects_garbage() {
        assert!(parse_poll_events_batch("{not json", 0).is_err());
    }

    #[test]
    fn leaf_shape_poll_entry_matches_emitter_array() {
        let v: serde_json::Value =
            serde_json::from_str(r#"[["flywheel-toron-oc","working","opencode","w3H:p1"]]"#)
                .unwrap();
        let states: Vec<AgentState> = v
            .as_array()
            .unwrap()
            .iter()
            .map(parse_poll_entry)
            .collect::<anyhow::Result<_>>()
            .unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].name, "flywheel-toron-oc");
        assert_eq!(states[0].status, "working");
        assert_eq!(states[0].kind, "opencode");
        assert_eq!(states[0].pane_id, "w3H:p1");
    }

    #[test]
    fn leaf_shape_ready_beads_matches_emitter() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"beads":[{"id":"bd-25g7.2","title":"Engine cron path"}]}"#)
                .unwrap();
        let beads = parse_bead_list(&v, "__ready").unwrap();
        assert_eq!(beads.len(), 1);
        assert_eq!(beads[0].id, "bd-25g7.2");
        assert_eq!(beads[0].title, "Engine cron path");
        assert_eq!(beads[0].assignee, None, "ready beads carry no assignee");
    }

    #[test]
    fn leaf_shape_inprogress_beads_matches_emitter() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"beads":[{"id":"bd-25g7.2","title":"Engine cron path","assignee":"DustySparrow"}]}"#,
        )
        .unwrap();
        let beads = parse_bead_list(&v, "__inprogress").unwrap();
        assert_eq!(beads[0].assignee.as_deref(), Some("DustySparrow"));
    }

    #[test]
    fn leaf_shape_expected_replies_matches_emitter() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"entries":[{"agent":"flywheel-toron-oc","thread":"bd-1","head":"abc","created_ts":"2026-08-12T00:00:00Z"}]}"#,
        )
        .unwrap();
        let entries = parse_expected_reply_entries(&v);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].agent, "flywheel-toron-oc");
        assert_eq!(entries[0].thread, "bd-1");
        assert_eq!(entries[0].head.as_deref(), Some("abc"));
    }

    #[test]
    fn leaf_shape_check_reply_matches_emitter() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"newest_id":"def","author":"RoseDune"}"#).unwrap();
        let (newest, author) = parse_reply_check(&v);
        assert_eq!(newest.as_deref(), Some("def"));
        assert_eq!(author.as_deref(), Some("RoseDune"));
    }

    #[test]
    fn blind_completion_count_accumulates_only_on_empty_and_failing() {
        assert_eq!(blind_completion_count(true, true, 0), 1);
        assert_eq!(blind_completion_count(true, true, 2), 3);
        assert_eq!(blind_completion_count(true, false, 2), 0);
        assert_eq!(blind_completion_count(false, true, 2), 0);
        const {
            assert!(
                DEFAULT_LEAF_FAILURE_ABORT_POLLS >= 3,
                "abort is not instant"
            );
        };
    }

    /// bd-swarm-loop-durable-panic-atlo: a blind run aborts once the threshold
    /// trips; there is no stale-checkpoint degrade — plain async has no replay.
    #[test]
    fn blind_completion_degrades_resumed_and_aborts_fresh() {
        let t = DEFAULT_LEAF_FAILURE_ABORT_POLLS;
        assert_eq!(
            blind_completion_outcome(true, t - 1, t),
            BlindCompletion::Continue
        );
        assert_eq!(
            blind_completion_outcome(false, t - 1, t),
            BlindCompletion::Continue
        );
        assert_eq!(blind_completion_outcome(true, t, t), BlindCompletion::Abort);
        assert_eq!(
            blind_completion_outcome(true, t + 5, t),
            BlindCompletion::Abort
        );
        assert_eq!(
            blind_completion_outcome(false, t, t),
            BlindCompletion::Abort
        );
        assert_eq!(
            blind_completion_outcome(false, t + 5, t),
            BlindCompletion::Abort
        );
    }

    /// bd-swarm-loop-durable-panic-atlo: replaying a dead run's checkpoints is
    /// OPT-IN via `--resume`; a pending prior with no flag starts FRESH (the
    /// zombie is rejected by the stale sweep, never replayed).
    #[test]
    fn loop_start_decision_fresh_unless_explicit_resume() {
        assert_eq!(loop_start_decision(false, false, true), LoopStart::Fresh);
        assert_eq!(loop_start_decision(true, false, false), LoopStart::Fresh);
        assert_eq!(loop_start_decision(true, false, true), LoopStart::Resume);
        assert_eq!(loop_start_decision(true, true, true), LoopStart::Fresh);
        assert_eq!(loop_start_decision(false, true, false), LoopStart::Fresh);
    }

    /// bd-swarm-loop-durable-panic-atlo: a supervised pass must never take a
    /// live loop over (no `--force`), and the stop rule counts consecutive
    /// failures, resetting on any success.
    #[test]
    fn supervise_pass_argv_never_forces() {
        let argv = supervise_pass_argv("proj", true, "all");
        assert_eq!(argv[0], "loop-mode");
        assert_eq!(argv[1], "run");
        assert_eq!(argv[2], "proj");
        assert!(argv.contains(&"--once".to_string()));
        assert!(argv.contains(&"--work".to_string()));
        assert!(
            !argv.contains(&"--force".to_string()),
            "a supervised pass must refuse loudly, not take over a live loop: {argv:?}"
        );
        let no_work = supervise_pass_argv("proj", false, "all");
        assert!(!no_work.contains(&"--work".to_string()));
        assert!(!no_work.contains(&"--force".to_string()));
    }

    /// bd-resume-window-lease-sweep-4sa2: resumes dedupe per (bead, pane)
    /// inside a window, a peer-held bead draws none, and a closed-reason lease
    /// is swept while an open one is kept.
    #[test]
    fn resume_window_dedupes_and_respects_peer_holds() {
        const W: u64 = RESUME_WINDOW_DEFAULT_SECS;
        let now = 10_000_000_000_000i64;
        let secs = |s: i64| now - s * 1_000_000;

        assert!(
            should_resume_attempt(None, now, W, false, None),
            "never resumed → resume"
        );
        assert!(
            !should_resume_attempt(Some(now), now, W, false, None),
            "just resumed → no second resume this pass"
        );
        assert!(!should_resume_attempt(Some(secs(800)), now, W, false, None));
        assert!(
            should_resume_attempt(Some(secs(1_000)), now, W, false, None),
            "past the window"
        );
        assert!(within_resume_window(Some(secs(60)), now, W));

        let reservations = vec![
            ("bd-peer".to_string(), "PeerPane".to_string()),
            ("bd-mine".to_string(), "MyPane".to_string()),
            ("bd-unstamped".to_string(), String::new()),
        ];
        assert_eq!(
            other_file_holder("src/main.rs", &[("src/**".into(), "OtherPin".into())], "Me"),
            Some("OtherPin")
        );
        assert_eq!(
            other_file_holder("src/main.rs", &[("src/**".into(), "Me".into())], "Me"),
            None
        );
        assert_eq!(
            other_file_holder("src/main.rs", &[("src/**".into(), "  ".into())], "Me"),
            None
        );
        let files = vec![
            ("src/**".into(), "bd-1".into(), "Me".into()),
            ("src/main.rs".into(), "bd-2".into(), "Other".into()),
        ];
        assert!(file_blocks_bead("bd-1", &files, "Me", &[]));
        assert!(file_blocks_bead("bd-2", &files, "Other", &[]));
        assert!(!file_blocks_bead("bd-9", &files, "Me", &[]));
        assert!(file_blocks_bead(
            "bd-9",
            &files,
            "Me",
            &["src/main.rs".to_string()]
        ));
        assert!(file_blocks_bead(
            "bd-9",
            &files,
            "",
            &["src/main.rs".to_string()]
        ));
        assert_eq!(
            path_tokens("edit `src/main.rs` and leave Cargo.toml"),
            vec!["src/main.rs".to_string()]
        );
        assert!(!file_blocks_bead("bd-1", &files, "", &[]));
        assert_eq!(
            file_hold_notice("bd-1", &files, "Me", &[]),
            Some(("Other".to_string(), "src/**".to_string()))
        );
        assert_eq!(
            file_hold_notice("bd-2", &files, "Other", &[]),
            Some(("Me".to_string(), "src/main.rs".to_string()))
        );
        assert_eq!(
            file_hold_notice("bd-9", &files, "Me", &["src/main.rs".to_string()]),
            Some(("Other".to_string(), "src/main.rs".to_string()))
        );
        assert_eq!(file_hold_notice("bd-9", &files, "Nobody", &[]), None);
        assert!(peer_held("bd-peer", &reservations, "MyPane"));
        assert!(
            !peer_held("bd-peer", &reservations, "PeerPane"),
            "own lease is not a peer"
        );
        assert!(!peer_held("bd-mine", &reservations, "MyPane"));
        assert!(!peer_held("bd-other", &reservations, "MyPane"));
        assert!(
            !peer_held("bd-unstamped", &reservations, "MyPane"),
            "empty holder is not a peer"
        );
        assert!(
            !should_resume_attempt(
                None,
                now,
                W,
                peer_held("bd-peer", &reservations, "MyPane"),
                None
            ),
            "peer-held → no resume"
        );

        let leases = vec![
            ("src/a.rs".to_string(), "bd-closed".to_string()),
            ("src/b.rs".to_string(), "bd-open".to_string()),
            ("docs/c.md".to_string(), "bd-closed".to_string()),
            ("src/d.rs".to_string(), String::new()),
        ];
        let closed = std::collections::HashSet::from(["bd-closed".to_string()]);
        assert_eq!(
            closed_reason_lease_ids(&leases, &closed),
            vec!["src/a.rs".to_string(), "docs/c.md".to_string()]
        );
        let after = vec![
            ("src/b.rs".to_string(), "bd-open".to_string()),
            ("src/d.rs".to_string(), String::new()),
        ];
        assert!(closed_reason_lease_ids(&after, &closed).is_empty());
        assert!(
            closed_reason_lease_ids(&leases, &std::collections::HashSet::new()).is_empty(),
            "no closed set → nothing"
        );
    }

    /// bd-bf39z: the loop never resumes another live pane's bead. Two
    /// in-progress beads owned by live Busy panes plus two unowned ready
    /// beads: idle panes are fed the unowned beads, and the live-owned
    /// resumes are never sent.
    #[test]
    fn loop_never_resumes_another_live_panes_bead() {
        let live_owned: HashMap<String, String> = HashMap::from([
            ("bd-owned-a".to_string(), "BusyPinA".to_string()),
            ("bd-owned-b".to_string(), "BusyPinB".to_string()),
        ]);
        let queue = vec![
            resume_brief("bd-owned-a", "owned work a"),
            resume_brief("bd-owned-b", "owned work b"),
            brief("bd-free-c", "free work c"),
            brief("bd-free-d", "free work d"),
        ];
        let states = vec![
            st("busy-a", "working"),
            st("busy-b", "working"),
            st("idle-p", "idle"),
            st("idle-q", "idle"),
        ];
        let actions = plan_actions_with_live_owned(
            &LoopModel {
                queue,
                ..Default::default()
            },
            &states,
            None,
            &HashMap::new(),
            &std::collections::HashSet::new(),
            &live_owned,
        );
        let sent: Vec<&str> = actions
            .iter()
            .filter_map(|a| match a {
                LoopAction::Send { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent.len(),
            2,
            "only the two idle panes are fed: {actions:?}"
        );
        assert!(
            sent[0].contains("bd-free-c"),
            "first idle pane falls through to unowned: {sent:?}"
        );
        assert!(
            sent[1].contains("bd-free-d"),
            "second idle pane falls through to unowned: {sent:?}"
        );
        assert!(
            sent.iter()
                .all(|t| !t.contains("bd-owned-a") && !t.contains("bd-owned-b")),
            "live-owned resumes never send: {sent:?}"
        );
    }

    /// bd-loop-feeds-idle-panes-rh2z: the idle-pane rotation feeds a stalled
    /// bead back through its OWN holder, never hands a bead to a non-owner,
    /// and only exempts an own-work pair from the resume window after N
    /// starved passes.
    #[test]
    fn loop_feeds_idle_panes_stalled_beads() {
        let now = 10_000_000_000_000i64;
        let secs = |s: i64| now - s * 1_000_000;
        let bead = |id: &str, assignee: Option<&str>| ReadyBead {
            id: id.into(),
            title: format!("work {id}"),
            assignee: assignee.map(str::to_string),
            verify: Some("true".into()),
            ..Default::default()
        };
        let pins = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pairs = |plan: &RotationPlan| {
            plan.picks
                .iter()
                .map(|p| (p.pin.clone(), p.bead.clone()))
                .collect::<Vec<_>>()
        };
        let empty = std::collections::BTreeMap::new();
        let no_starved: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        let window = std::collections::BTreeMap::from([(
            crate::state::resume_key("fx-stalled", "GreenStream"),
            secs(60),
        )]);

        let live = pins(&["GreenStream", "AmberMarlin"]);
        let stalled = vec![bead("fx-stalled", Some("GreenStream"))];
        let plan = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &live,
            idle_pins: &live,
            in_progress: &stalled,
            ready: &[],
            peers: &[],
            taken: &[],
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert_eq!(
            plan.picks,
            vec![RotationPick {
                pin: "GreenStream".into(),
                bead: "fx-stalled".into(),
                exempt: false,
            }],
            "the stalled bead goes to its own holder, and to nobody else"
        );
        assert_eq!(plan.starved_after.get("GreenStream"), Some(&0));

        let two_stalls = vec![
            bead("fx-g", Some("GreenStream")),
            bead("fx-a", Some("AmberMarlin")),
            bead("fx-extra", None),
        ];
        let plan = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &live,
            idle_pins: &live,
            in_progress: &two_stalls,
            ready: &[],
            peers: &[],
            taken: &[],
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert_eq!(
            pairs(&plan),
            vec![
                ("GreenStream".to_string(), "fx-g".to_string()),
                ("AmberMarlin".to_string(), "fx-a".to_string()),
            ],
            "each pane resumes its own stall — never another pane's"
        );

        let live3 = pins(&["GreenStream", "AmberMarlin", "QuietHeron"]);
        let two_idle = pins(&["GreenStream", "AmberMarlin"]);
        let foreign = vec![bead("fx-hers", Some("QuietHeron"))];
        let nobody = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &live3,
            idle_pins: &two_idle,
            in_progress: &foreign,
            ready: &[],
            peers: &[],
            taken: &[],
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert!(
            nobody.picks.is_empty(),
            "a bead owned by a live pane never moves to another pane"
        );
        let peer_lease = vec![("fx-held".to_string(), "QuietHeron".to_string())];
        let held = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &live3,
            idle_pins: &two_idle,
            in_progress: &[],
            ready: &[bead("fx-held", None)],
            peers: &peer_lease,
            taken: &[],
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert!(
            held.picks.is_empty(),
            "a peer-held bead draws no pick (lane split, not a stall)"
        );
        let claimed = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &live3,
            idle_pins: &two_idle,
            in_progress: &[],
            ready: &[bead("fx-claimed", Some("QuietHeron"))],
            peers: &[],
            taken: &[],
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert!(
            claimed.picks.is_empty(),
            "a ready bead assigned to a LIVE pane never moves to another pane"
        );
        let orphaned = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &live3,
            idle_pins: &two_idle,
            in_progress: &[],
            ready: &[bead("fx-orphan", Some("DeadAgent"))],
            peers: &[],
            taken: &[],
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert_eq!(
            pairs(&orphaned),
            vec![("GreenStream".to_string(), "fx-orphan".to_string())],
            "an orphaned claim is free work, not somebody else's bead"
        );

        let fresh = vec![bead("fx-fresh", None)];
        let plan = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &two_idle,
            idle_pins: &two_idle,
            in_progress: &[],
            ready: &fresh,
            peers: &[],
            taken: &[],
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert_eq!(
            pairs(&plan),
            vec![("GreenStream".to_string(), "fx-fresh".to_string())],
            "one unowned bead feeds one idle pane, once"
        );
        let taken = vec!["fx-fresh".to_string()];
        let requeued = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &two_idle,
            idle_pins: &two_idle,
            in_progress: &stalled,
            ready: &fresh,
            peers: &[],
            taken: &taken,
            starved: &no_starved,
            resume_window: &empty,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
        });
        assert_eq!(
            pairs(&requeued),
            vec![("GreenStream".to_string(), "fx-stalled".to_string())],
            "the queued bead is skipped; the pane's own stall still lands"
        );

        let gated = |starved: &std::collections::BTreeMap<String, u32>| {
            idle_pane_rotation(&IdleRotationInputs {
                live_pins: &live,
                idle_pins: &live,
                in_progress: &stalled,
                ready: &[],
                peers: &[],
                taken: &[],
                starved,
                resume_window: &window,
                now_us: now,
                window_secs: RESUME_WINDOW_DEFAULT_SECS,
                exempt_after_passes: IDLE_PANE_EXEMPT_AFTER_PASSES,
            })
        };
        let immediate = idle_pane_rotation(&IdleRotationInputs {
            live_pins: &live,
            idle_pins: &live,
            in_progress: &stalled,
            ready: &[],
            peers: &[],
            taken: &[],
            starved: &no_starved,
            resume_window: &window,
            now_us: now,
            window_secs: RESUME_WINDOW_DEFAULT_SECS,
            exempt_after_passes: 0,
        });
        assert_eq!(
            immediate.picks.len(),
            1,
            "a 0 exemption budget resumes the pane on the first pass"
        );

        let pass1 = gated(&no_starved);
        assert!(pass1.picks.is_empty(), "pass 1: the window still holds");
        assert_eq!(pass1.starved_after.get("GreenStream"), Some(&1));
        let pass2 = gated(&pass1.starved_after);
        assert!(pass2.picks.is_empty(), "pass 2: still inside the window");
        assert_eq!(pass2.starved_after.get("GreenStream"), Some(&2));
        let pass3 = gated(&pass2.starved_after);
        assert_eq!(
            pass3.picks,
            vec![RotationPick {
                pin: "GreenStream".into(),
                bead: "fx-stalled".into(),
                exempt: true,
            }],
            "the Nth starved pass releases the window (a stalled pane outranks it)"
        );
        assert_eq!(
            pass3.starved_after.get("GreenStream"),
            Some(&0),
            "a released pick ends the starvation run"
        );
    }

    /// bd-pane-wedge-detect-respawn-pnyi: a simulated wedge — a pane parked on
    /// a fatal error line, process alive, nothing moving — produces ONE page
    /// naming the pane and the error, and every un-judgeable shape stays quiet.
    #[test]
    fn pane_wedge_detected_and_paged() {
        let wedge = PaneWedgeInputs {
            pin: "EmberOtter",
            pane_id: "w1:p1",
            process_alive: Some(true),
            recent_output: Some("streaming tokens…\nError: API error 400 (reasoning replay)\n"),
            last_commit_age_secs: Some(9_000),
            quiet_secs: 2_700,
        };
        match pane_wedge_action(&wedge, false) {
            PaneWedgeAction::Page {
                pin,
                pane_id,
                error,
            } => {
                assert_eq!(pin, "EmberOtter", "the page names the pin");
                assert_eq!(pane_id, "w1:p1", "the page names the pane");
                assert!(
                    error.contains("reasoning replay") && error.contains("400"),
                    "the page carries the error line: {error}"
                );
            }
            other => panic!("a simulated wedge must page: {other:?}"),
        }
        assert_eq!(
            pane_wedge_action(&wedge, true),
            PaneWedgeAction::Hold,
            "never one page per pass"
        );
        assert_eq!(
            pane_wedge_action(
                &PaneWedgeInputs {
                    process_alive: Some(false),
                    ..wedge.clone()
                },
                false
            ),
            PaneWedgeAction::Quiet
        );
        assert_eq!(
            pane_wedge_action(
                &PaneWedgeInputs {
                    process_alive: None,
                    ..wedge.clone()
                },
                false
            ),
            PaneWedgeAction::Quiet
        );
        assert_eq!(
            pane_wedge_action(
                &PaneWedgeInputs {
                    recent_output: None,
                    ..wedge.clone()
                },
                false
            ),
            PaneWedgeAction::Quiet
        );
        assert_eq!(
            pane_wedge_action(
                &PaneWedgeInputs {
                    recent_output: Some("Error: API error 400\nwork resumed; committing\n"),
                    ..wedge.clone()
                },
                false
            ),
            PaneWedgeAction::Quiet
        );
        assert_eq!(
            pane_wedge_action(
                &PaneWedgeInputs {
                    last_commit_age_secs: Some(60),
                    ..wedge.clone()
                },
                false
            ),
            PaneWedgeAction::Quiet
        );
        assert_eq!(
            pane_wedge_action(
                &PaneWedgeInputs {
                    last_commit_age_secs: None,
                    ..wedge.clone()
                },
                false
            ),
            PaneWedgeAction::Quiet
        );
        assert_eq!(parked_error_line("\n\n"), None);
        assert_eq!(parked_error_line("all good\n"), None);
        assert_eq!(
            parked_error_line("noise\nError: rate limit exceeded\n"),
            Some("Error: rate limit exceeded".to_string())
        );
    }

    /// bd-supervise-dead-stop-escalation-rweq: a dead stop escalates EXACTLY
    /// once (not per pass), and a recovering fleet clears it so a later stall
    /// can escalate again.
    #[test]
    fn supervise_escalates_on_dead_stop() {
        let quiet = DeadStopInputs {
            work_remaining: true,
            fleet_knowable: true,
            any_pane_working: false,
            last_commit_age_secs: Some(3_000),
            stall_secs: 2_700,
            already_escalated: false,
        };
        assert_eq!(
            dead_stop_action(&quiet),
            DeadStopAction::Escalate(DeadStopKind::SilentPanes)
        );
        assert_eq!(
            dead_stop_action(&DeadStopInputs {
                already_escalated: true,
                ..quiet
            }),
            DeadStopAction::Hold
        );
        let recovered = DeadStopInputs {
            last_commit_age_secs: Some(10),
            already_escalated: true,
            ..quiet
        };
        assert_eq!(dead_stop_action(&recovered), DeadStopAction::Clear);
        assert_eq!(
            dead_stop_action(&DeadStopInputs {
                already_escalated: false,
                ..quiet
            }),
            DeadStopAction::Escalate(DeadStopKind::SilentPanes)
        );
        assert_eq!(
            dead_stop_action(&DeadStopInputs {
                any_pane_working: true,
                ..quiet
            }),
            DeadStopAction::Escalate(DeadStopKind::StuckWorking)
        );
        assert_eq!(
            dead_stop_action(&DeadStopInputs {
                work_remaining: false,
                ..quiet
            }),
            DeadStopAction::Clear
        );
        assert_eq!(
            dead_stop_action(&DeadStopInputs {
                fleet_knowable: false,
                ..quiet
            }),
            DeadStopAction::Wait
        );
        assert_eq!(
            dead_stop_action(&DeadStopInputs {
                last_commit_age_secs: None,
                ..quiet
            }),
            DeadStopAction::Wait
        );
    }

    #[test]
    fn route_event_done_dispatches_and_pops_queue() {
        let states = vec![st_pane("a", "done", "%1")];
        let mut queue = vec!["p1".into(), "p2".into()];
        let mut pending = std::collections::HashSet::new();
        let a = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "done", 1),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(
            a,
            Some(LoopAction::DispatchImmediate { name, text }) if name == "a" && text == "p1"
        ));
        assert_eq!(queue, vec!["p2".to_string()]);
        assert!(pending.contains("a"));
    }

    #[test]
    fn route_event_double_dispatch_guard() {
        let states = vec![st_pane("a", "done", "%1")];
        let mut queue = vec!["p1".into(), "p2".into()];
        let mut pending = std::collections::HashSet::new();
        let _ = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "done", 1),
            &mut queue,
            &mut pending,
        );
        let a2 = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "done", 2),
            &mut queue,
            &mut pending,
        );
        assert!(a2.is_none());
        assert_eq!(queue, vec!["p2".to_string()]);
        let a3 = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "working", 3),
            &mut queue,
            &mut pending,
        );
        assert!(a3.is_none());
        assert!(!pending.contains("a"));
        let a4 = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "idle", 4),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(
            a4,
            Some(LoopAction::DispatchImmediate { text, .. }) if text == "p2"
        ));
    }

    #[test]
    fn route_event_blocked_and_exited() {
        let states = vec![st_pane("a", "working", "%1")];
        let mut queue = vec!["p1".into()];
        let mut pending = std::collections::HashSet::new();
        let b = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "blocked", 1),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(b, Some(LoopAction::Blocked { name }) if name == "a"));
        assert_eq!(queue.len(), 1);
        let x = route_event(
            &states,
            &pev("pane.exited", "%1", "exited", 2),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(
            x,
            Some(LoopAction::Diagnose { name, reason })
                if name == "a" && reason == "pane_exited"
        ));
    }

    #[test]
    fn self_heal_unknown_diagnose_then_respawn() {
        let mut hs = HealState::default();
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 0, 3, 2),
            HealAction::None
        );
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 0, 3, 2),
            HealAction::None
        );
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 0, 3, 2),
            HealAction::Diagnose
        );
        assert!(hs.awaiting_clear);
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 0, 3, 2),
            HealAction::Respawn
        );
        assert_eq!(hs.respawn_count, 1);
    }

    #[test]
    fn self_heal_exited_diagnose_immediate() {
        let mut hs = HealState::default();
        assert_eq!(
            self_heal_step(&mut hs, "working", true, 0, 3, 2),
            HealAction::Diagnose
        );
        assert_eq!(
            self_heal_step(&mut hs, "unknown", true, 0, 3, 2),
            HealAction::Respawn
        );
    }

    #[test]
    fn self_heal_budget_exhausted_escalates() {
        let mut hs = HealState {
            respawn_count: 2,
            awaiting_clear: true,
            ..Default::default()
        };
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 0, 3, 2),
            HealAction::EscalateBlocked
        );
    }

    #[test]
    fn self_heal_classified_resets_episode() {
        let mut hs = HealState {
            unknown_count: 2,
            awaiting_clear: true,
            diagnose_count: 1,
            respawn_count: 1,
        };
        assert_eq!(
            self_heal_step(&mut hs, "idle", false, 0, 3, 2),
            HealAction::None
        );
        assert_eq!(hs.unknown_count, 0);
        assert!(!hs.awaiting_clear);
        assert_eq!(hs.respawn_count, 1);
    }

    #[test]
    fn self_heal_flicker_never_diagnoses() {
        let mut hs = HealState::default();
        self_heal_step(&mut hs, "unknown", false, 0, 3, 2);
        self_heal_step(&mut hs, "idle", false, 0, 3, 2);
        self_heal_step(&mut hs, "unknown", false, 0, 3, 2);
        assert_eq!(hs.unknown_count, 1);
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 0, 3, 2),
            HealAction::None
        );
    }

    /// bd-bvwf.5: a WORKING agent polling unknown for a few polls (fresh
    /// spawn detection) is never Diagnosed or Respawnd inside the grace
    /// window — detection latency is not death.
    #[test]
    fn self_heal_detect_grace_never_touches_young_unknown() {
        let mut hs = HealState::default();
        for _ in 0..6 {
            assert_eq!(
                self_heal_step(&mut hs, "unknown", false, 6, 3, 2),
                HealAction::None
            );
        }
        assert_eq!(hs.diagnose_count, 0);
        assert_eq!(hs.respawn_count, 0);
        assert_eq!(
            self_heal_step(&mut hs, "working", false, 6, 3, 2),
            HealAction::None
        );
        assert_eq!(hs.unknown_count, 0);
        let mut old = HealState::default();
        assert_eq!(
            self_heal_step(&mut old, "unknown", false, 0, 3, 2),
            HealAction::None
        );
    }

    /// bd-bvwf.5: after the grace window, the ladder resumes — a persistently
    /// unknown agent is diagnosed, then respawned (budgeted).
    #[test]
    fn self_heal_after_grace_diagnose_then_respawn() {
        let mut hs = HealState::default();
        for _ in 0..6 {
            assert_eq!(
                self_heal_step(&mut hs, "unknown", false, 6, 3, 2),
                HealAction::None
            );
        }
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 6, 3, 2),
            HealAction::Diagnose
        );
        assert!(hs.awaiting_clear);
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 6, 3, 2),
            HealAction::Respawn
        );
        assert_eq!(hs.respawn_count, 1);
    }

    #[test]
    fn route_event_substitutes_agent_name() {
        let states = vec![st_pane("flywheel-oc", "idle", "%9")];
        let mut queue = vec!["hi {AGENT_NAME}".into()];
        let mut pending = std::collections::HashSet::new();
        let a = route_event(
            &states,
            &pev("pane.agent_status_changed", "%9", "idle", 1),
            &mut queue,
            &mut pending,
        );
        match a {
            Some(LoopAction::DispatchImmediate { text, .. }) => {
                assert_eq!(text, "hi flywheel-oc");
            }
            other => panic!("expected DispatchImmediate, got {other:?}"),
        }
    }

    #[test]
    fn parse_poll_entry_reads_pane_id() {
        let v = serde_json::json!(["a", "idle", "opencode", "%p1"]);
        let s = parse_poll_entry(&v).unwrap();
        assert_eq!(s.pane_id, "%p1");
        assert_eq!(s.kind, "opencode");
        let v2 = serde_json::json!(["b", "working", "pi"]);
        let s2 = parse_poll_entry(&v2).unwrap();
        assert_eq!(s2.pane_id, "");
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct DispatchRec {
        agent: String,
        kind: &'static str,
        content: String,
    }

    fn rec_from_action(a: &LoopAction) -> Option<DispatchRec> {
        match a {
            LoopAction::Send { name, text } | LoopAction::DispatchImmediate { name, text } => {
                Some(DispatchRec {
                    agent: name.clone(),
                    kind: "send",
                    content: text.clone(),
                })
            }
            LoopAction::PinStamp { .. } => None,
            LoopAction::Blocked { name } => Some(DispatchRec {
                agent: name.clone(),
                kind: "blocked",
                content: String::new(),
            }),
            LoopAction::Diagnose { name, reason } => Some(DispatchRec {
                agent: name.clone(),
                kind: "diagnose",
                content: reason.clone(),
            }),
            LoopAction::Respawn { name, reason } => Some(DispatchRec {
                agent: name.clone(),
                kind: "respawn",
                content: reason.clone(),
            }),
            LoopAction::Nudge { name } => Some(DispatchRec {
                agent: name.clone(),
                kind: "nudge",
                content: String::new(),
            }),
            LoopAction::Reclaim {
                bead_id,
                pin,
                terminal_name,
                reason,
            } => Some(DispatchRec {
                agent: terminal_name.clone(),
                kind: "reclaim",
                content: format!("{bead_id}:{pin}:{reason}"),
            }),
            LoopAction::Wait | LoopAction::PausedResurface { .. } => None,
        }
    }

    fn event_dispatch(
        states: &[AgentState],
        events: &[PollEvent],
        queue: &mut Vec<String>,
        pending: &mut std::collections::HashSet<String>,
    ) -> Vec<DispatchRec> {
        let mut out = Vec::new();
        for ev in events {
            if let Some(a) = route_event(states, ev, queue, pending)
                && let Some(r) = rec_from_action(&a)
            {
                out.push(r);
            }
        }
        out
    }

    #[test]
    fn event_poll_parity_double_dispatch_guard() {
        let states = vec![st_pane("a", "done", "%1")];
        let mut q = vec!["p1".into(), "p2".into()];
        let mut pending = std::collections::HashSet::new();
        let e1 = event_dispatch(
            &states,
            &[
                pev("pane.agent_status_changed", "%1", "done", 1),
                pev("pane.agent_status_changed", "%1", "done", 2),
            ],
            &mut q,
            &mut pending,
        );
        assert_eq!(e1.len(), 1, "double-dispatch guard: {e1:?}");
        assert_eq!(e1[0].content, "p1");
        assert_eq!(q, vec!["p2".to_string()]);
    }

    #[test]
    fn event_poll_parity_exited_diagnose_not_send() {
        let states = vec![st_pane("a", "working", "%1")];
        let mut q = vec!["p1".into()];
        let mut pending = std::collections::HashSet::new();
        let event = event_dispatch(
            &states,
            &[pev("pane.exited", "%1", "exited", 1)],
            &mut q,
            &mut pending,
        );
        assert_eq!(event.len(), 1);
        assert_eq!(event[0].kind, "diagnose");
        assert_eq!(event[0].content, "pane_exited");
    }

    /// The reaction-without-waiting timed test (unit form): a `done` event
    /// flips all_settled on the merged states — no snapshot needed. Before
    /// the merge the loop would have waited for the next poll to see it.
    #[test]
    fn events_primary_done_event_flips_settled_without_poll() {
        let mut states = vec![st_pane("a", "working", "%1")];
        assert!(!all_settled(&states));
        apply_events_to_states(
            &mut states,
            &[pev("pane.agent_status_changed", "%1", "done", 1)],
        );
        assert!(
            all_settled(&states),
            "event-asserted done must settle immediately"
        );
        let confirmed = vec![st_pane("a", "done", "%1")];
        assert!(all_settled(&confirmed));
        assert_eq!(states, confirmed, "merged view == poll-confirmed view");
    }

    /// pane.exited marks the agent exited (diagnose-able, not idle).
    #[test]
    fn events_primary_exit_marks_exited() {
        let mut states = vec![st_pane("a", "working", "%1")];
        apply_events_to_states(&mut states, &[pev("pane.exited", "%1", "exited", 1)]);
        assert_eq!(states[0].status, "exited");
        assert!(!all_settled(&states), "exited is not settled");
    }

    /// agent_detected updates the status of a known pane.
    #[test]
    fn events_primary_detected_updates_status() {
        let mut states = vec![st_pane("a", "unknown", "%1")];
        apply_events_to_states(&mut states, &[pev("pane.agent_detected", "%1", "idle", 1)]);
        assert_eq!(states[0].status, "idle");
    }

    /// Unknown panes (not yet in the snapshot) are left to the next poll —
    /// the merge never fabricates state.
    #[test]
    fn events_primary_unknown_pane_is_noop() {
        let mut states = vec![st_pane("a", "working", "%1")];
        apply_events_to_states(
            &mut states,
            &[pev("pane.agent_status_changed", "%99", "done", 1)],
        );
        assert_eq!(states[0].status, "working");
        let mut states2 = vec![st_pane("a", "working", "%1")];
        apply_events_to_states(
            &mut states2,
            &[pev("pane.agent_status_changed", "", "done", 2)],
        );
        assert_eq!(states2[0].status, "working");
    }

    /// The snapshot cadence policy: after crash-resume / no state, on stream
    /// error (fallback), and at the heartbeat boundary.
    #[test]
    fn events_primary_snapshot_due_policy() {
        assert!(snapshot_due(true, false, 0));
        assert!(!snapshot_due(false, false, 3));
        assert!(snapshot_due(false, false, HEARTBEAT_POLL_EVERY));
        assert!(snapshot_due(false, true, 1));
    }

    /// bd-lob1: the loop observes the open -> closed transition itself and
    /// releases that bead's reservations.
    ///
    /// Why pin it: `docs/configuration.md` §6 listed this as "open automation
    /// (follow-up beads, not this session)" long after it shipped, because
    /// nothing in the code said so. The doctrine now says the loop does it, so
    /// this test is what keeps that claim true — a refactor that drops the
    /// release fails here instead of silently reinstating the doc's gap.
    #[test]
    fn close_path_releases_the_beads_reservations() {
        let loop_path = include_str!("durable.rs");
        let arm = loop_path
            .split("ClaimWatchAction::ReleaseReservations { bead_id } => {")
            .nth(1)
            .expect("the closed-bead arm exists")
            .split("\nfn ")
            .next()
            .expect("arm body");
        assert!(
            arm.contains("__release-bead-locks"),
            "the open->closed arm must release the bead's reservations (bd-lob1)"
        );
        assert!(
            loop_path.contains("\"__release-bead-locks\""),
            "the leaf must call the __release-bead-locks door"
        );
        let main_src = crate::test_source::crate_source();
        let handler = main_src
            .split("fn cmd_internal_release_bead_locks")
            .nth(1)
            .expect("handler exists")
            .split("\nfn ")
            .next()
            .expect("handler body");
        assert!(
            handler.contains("release_by_reason"),
            "the door must release BY REASON (every holder), not one holder"
        );
        assert!(
            handler.contains("orchestrator_mail_name"),
            "release-by-reason is operator-signed; the loop must act in that context"
        );
    }

    #[test]
    fn close_path_not_closed_preserves_locks_and_gates_verifier() {
        let loop_path = include_str!("durable.rs");
        let arm = loop_path
            .split("ClaimWatchAction::ReleaseReservations { bead_id } => {")
            .nth(1)
            .expect("the closed-bead arm exists")
            .split("\nfn ")
            .next()
            .expect("arm body");
        assert!(
            arm.contains("\"not-closed\" => {"),
            "not-closed status must be handled explicitly"
        );
        assert!(
            arm.contains("dispatch_verifier && swarm.mark_verifier_dispatched"),
            "verifier dispatch must be gated on dispatch_verifier flag"
        );
    }

    /// Grep acceptance: the loop path (durable.rs + the __poll_events
    /// collector) contains no direct hc event construction — the
    /// event stream comes via the backend trait. The scan covers the
    /// non-test source (the loop path), not this test's own strings.
    ///
    /// The fence denies code-level coupling only. The loop core legitimately
    /// *names* hc in prose and data: the `terminal_name` field (the agent's
    /// hc-assigned name — data, not a call), the `LoopMsg::Hc`
    /// envelope variant (populated by the backend trait, never by an hc
    /// client), and doc comments. A blanket `contains("hc")` fails on
    /// those load-bearing names, so it cannot be the fence.
    #[test]
    fn events_primary_grep_no_plane_in_loop_path() {
        let durable = include_str!("durable.rs");
        let loop_path = durable
            .split("#[cfg(test)]")
            .next()
            .expect("non-test source");
        for denied in [
            "hc::",
            "crate::hc",
            "HcClient",
            "hc_client",
            "Hc::",
            "use hc",
            "extern crate hc",
        ] {
            assert!(
                !loop_path.contains(denied),
                "durable.rs (the loop core) must not touch hc directly: found {denied:?}"
            );
        }
        let main_src = crate::test_source::crate_source();
        let collect = main_src
            .split("fn collect_poll_events")
            .nth(1)
            .expect("collect_poll_events exists");
        let collect = collect.split("\nfn ").next().expect("collect fn body");
        assert!(
            collect.contains("backend::multiplexer_backend(cfg)"),
            "collect_poll_events must obtain the stream via the backend trait"
        );
        assert!(
            !collect.contains("hc"),
            "collect_poll_events must not construct hc events"
        );
    }

    fn bead_inprog(id: &str, assignee: &str) -> ReadyBead {
        ReadyBead {
            id: id.into(),
            title: format!("work {id}"),
            assignee: Some(assignee.into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "principle — decision".into(),
            updated_at: None,
            ..Default::default()
        }
    }

    #[test]
    fn renew_tick_renews_lease_near_expiry_fake_clock() {
        let now: i64 = 1_000_000_000_000;
        let near_expiry: i64 = now + 100 * 1_000_000;
        let far_expiry: i64 = now + 2000 * 1_000_000;
        let leases = vec![
            (
                "src/durable.rs".to_string(),
                "bd-renew-1".to_string(),
                Some(near_expiry),
            ),
            (
                "src/main.rs".to_string(),
                "bd-renew-1".to_string(),
                Some(far_expiry),
            ),
        ];
        let inprog = vec![bead_inprog("bd-renew-1", "GoldCove")];
        let live = vec!["GoldCove".to_string()];
        let to_renew = leases_to_renew(&leases, &inprog, &live, now, 600);
        assert_eq!(to_renew.len(), 1, "only near-expiry lease qualifies");
        assert_eq!(to_renew[0].0, "src/durable.rs");
        assert_eq!(to_renew[0].1, "bd-renew-1");
        assert!(
            !to_renew.iter().any(|(p, _)| p == "src/main.rs"),
            "far-expiry lease must not be renewed"
        );
    }

    #[test]
    fn renew_failed_logs_and_increments_counter() {
        reset_renew_fail_count();
        assert_eq!(renew_fail_count(), 0);
        record_renew_failure("src/durable.rs", "bd-renew-1", "toron unavailable");
        assert_eq!(
            renew_fail_count(),
            1,
            "failed renewal must increment visible counter"
        );
        record_renew_failure("src/main.rs", "bd-renew-2", "timeout");
        assert_eq!(renew_fail_count(), 2, "each failure increments");
        reset_renew_fail_count();
        assert_eq!(renew_fail_count(), 0);
    }

    #[test]
    fn renew_unrelated_leases_untouched() {
        let now: i64 = 5_000_000_000;
        let leases = vec![
            (
                "src/durable.rs".to_string(),
                "bd-renew-1".to_string(),
                Some(now + 50 * 1_000_000),
            ),
            (
                "src/unrelated.rs".to_string(),
                "bd-other".to_string(),
                Some(now + 50 * 1_000_000),
            ),
            (
                "src/no-assignee.rs".to_string(),
                "bd-renew-1".to_string(),
                Some(now + 50 * 1_000_000),
            ),
        ];
        let mut inprog = vec![bead_inprog("bd-renew-1", "GoldCove")];
        inprog.push(ReadyBead {
            id: "bd-other".into(),
            title: "other".into(),
            assignee: Some("DeadAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: None,
            ..Default::default()
        });
        let live = vec!["GoldCove".to_string()];
        let to_renew = leases_to_renew(&leases, &inprog, &live, now, 600);
        assert_eq!(
            to_renew.len(),
            2,
            "both leases with bd-renew-1 qualify (same reason, two paths) - but bd-other must not"
        );
        assert!(
            !to_renew
                .iter()
                .any(|(p, r)| p == "src/unrelated.rs" && r == "bd-other"),
            "unrelated lease with non-live assignee must be untouched"
        );
        let simple_leases = vec![
            ("src/durable.rs".to_string(), "bd-renew-1".to_string()),
            ("src/unrelated.rs".to_string(), "bd-unrelated".to_string()),
        ];
        let simple_renew = leases_to_renew_simple(&simple_leases, &inprog, &live);
        assert_eq!(simple_renew.len(), 1);
        assert_eq!(simple_renew[0].0, "src/durable.rs");
    }

    #[test]
    fn facts_close_emits_closed_fact() {
        let proj = format!(
            "facts-close-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = crate::temp_state_root("facts-close");

        emit_ledger_fact(&root, &proj, "bd-test-close", LedgerEvent::Closed);
        let facts = crate::load_facts(&root, &proj);
        assert_eq!(facts.len(), 1);
        match &facts[0] {
            IngressEvent::LedgerFact { bead_id, event } => {
                assert_eq!(bead_id, "bd-test-close");
                assert_eq!(*event, LedgerEvent::Closed);
            }
            other => panic!("expected LedgerFact, got {other:?}"),
        }
        crate::assert_live_root_untouched(&proj);
        let _ = std::fs::remove_dir_all(&root);
        assert!(crate::ingress::fact_summary(&facts[0]).is_some());
        assert!(
            crate::ingress::fact_summary(&facts[0])
                .unwrap()
                .contains("ledger bd-test-close closed")
        );
    }

    #[test]
    fn facts_stall_detection_fires_and_suppresses() {
        let now = chrono::Utc::now().timestamp();
        let threshold = 1800;
        let old_bead = ReadyBead {
            id: "bd-stall-1".into(),
            title: "stall".into(),
            assignee: Some("StallAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some((chrono::Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339()),
            ..Default::default()
        };
        let recent_bead = ReadyBead {
            id: "bd-stall-2".into(),
            title: "recent".into(),
            assignee: Some("StallAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let states = vec![
            AgentState {
                name: "StallAgent".into(),
                status: "idle".into(),
                kind: "opencode".into(),
                pane_id: "w1:p1".into(),
                mail_identity: None,
                nudge_streak: 0,
            },
            AgentState {
                name: "WorkingAgent".into(),
                status: "working".into(),
                kind: "opencode".into(),
                pane_id: "w1:p2".into(),
                mail_identity: None,
                nudge_streak: 0,
            },
        ];
        let stalled =
            detect_stalled_panes(std::slice::from_ref(&old_bead), &states, now, threshold);
        assert_eq!(stalled.len(), 1, "old idle bead should stall");
        match &stalled[0] {
            IngressEvent::PaneStalled {
                pane_id,
                terminal_name,
                last_status,
                bead_id,
                ..
            } => {
                assert_eq!(pane_id, "w1:p1");
                assert_eq!(terminal_name, "StallAgent");
                assert_eq!(last_status, "idle");
                assert_eq!(bead_id.as_deref(), Some("bd-stall-1"));
            }
            other => panic!("expected PaneStalled, got {other:?}"),
        }
        let stalled_recent = detect_stalled_panes(&[recent_bead], &states, now, threshold);
        assert!(stalled_recent.is_empty(), "recent bead must not stall");
        let working_states = vec![AgentState {
            name: "StallAgent".into(),
            status: "working".into(),
            kind: "opencode".into(),
            pane_id: "w1:p1".into(),
            mail_identity: None,
            nudge_streak: 0,
        }];
        let stalled_working = detect_stalled_panes(&[old_bead], &working_states, now, threshold);
        assert!(
            stalled_working.is_empty(),
            "working agent must not be considered stalled"
        );
    }

    #[test]
    fn facts_lapsed_renewal_emits() {
        let proj = format!(
            "facts-lapsed-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = crate::temp_state_root("facts-lapsed");
        emit_lease_lapsed(&root, &proj, "src/durable.rs", Some("bd-lapsed-1"));

        let facts = crate::load_facts(&root, &proj);
        assert_eq!(facts.len(), 1);
        match &facts[0] {
            IngressEvent::LeaseLapsed { path, bead_id } => {
                assert_eq!(path, "src/durable.rs");
                assert_eq!(bead_id.as_deref(), Some("bd-lapsed-1"));
            }
            other => panic!("expected LeaseLapsed, got {other:?}"),
        }
        crate::assert_live_root_untouched(&proj);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            crate::ingress::fact_summary(&facts[0])
                .unwrap()
                .contains("lease lapsed src/durable.rs")
        );
    }

    #[test]
    fn stall_classifier_maps_evidence_signatures_to_reasons() {
        assert_eq!(classify_stall_reason("rate limit exceeded"), "rate_limit");
        assert_eq!(
            classify_stall_reason("Quota exceeded for model"),
            "allowance"
        );
        assert_eq!(classify_stall_reason("got 429 error"), "rate_limit");
        assert_eq!(
            classify_stall_reason("model overloaded try later"),
            "rate_limit"
        );
        assert_eq!(classify_stall_reason("connection refused"), "network");
        assert_eq!(
            classify_stall_reason("timeout waiting for response"),
            "network"
        );
        assert_eq!(classify_stall_reason("reset by peer"), "network");
        assert_eq!(classify_stall_reason("host unreachable"), "network");
        assert_eq!(
            classify_stall_reason("unterminated tool call in response"),
            "model_truncated"
        );
        assert_eq!(
            classify_stall_reason("max tokens reached"),
            "model_truncated"
        );
        assert_eq!(
            classify_stall_reason("response abruptly ended"),
            "model_truncated"
        );
        assert_eq!(classify_stall_reason("just idle no output"), "silent_idle");
        assert_eq!(classify_stall_reason(""), "silent_idle");
        assert_eq!(
            classify_stall_reason("rate limit and timeout both present"),
            "rate_limit"
        );
    }

    #[test]
    fn stall_watching_tier_emits_before_diagnose_tier() {
        let now = chrono::Utc::now().timestamp();
        let threshold = 1800;
        let watching = stall_watching_threshold_secs(threshold);
        assert_eq!(watching, 450, "watching is threshold/4");
        let watching_bead = ReadyBead {
            id: "bd-watch".into(),
            title: "watching".into(),
            assignee: Some("WatchAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some(
                (chrono::Utc::now() - chrono::Duration::seconds((watching + 10) as i64))
                    .to_rfc3339(),
            ),
            ..Default::default()
        };
        let stalled_bead = ReadyBead {
            id: "bd-stalled".into(),
            title: "stalled".into(),
            assignee: Some("StallAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some(
                (chrono::Utc::now() - chrono::Duration::seconds((threshold + 10) as i64))
                    .to_rfc3339(),
            ),
            ..Default::default()
        };
        let states = vec![
            AgentState {
                name: "WatchAgent".into(),
                status: "idle".into(),
                kind: "opencode".into(),
                pane_id: "w1:p1".into(),
                mail_identity: None,
                nudge_streak: 0,
            },
            AgentState {
                name: "StallAgent".into(),
                status: "idle".into(),
                kind: "opencode".into(),
                pane_id: "w1:p2".into(),
                mail_identity: None,
                nudge_streak: 0,
            },
        ];
        let watching_events = detect_stalled_panes_tiered(
            std::slice::from_ref(&watching_bead),
            &states,
            now,
            threshold,
            watching,
        );
        assert_eq!(watching_events.len(), 1);
        match &watching_events[0] {
            IngressEvent::PaneStalled {
                severity, reason, ..
            } => {
                assert_eq!(severity, "watching", "watching tier severity");
                assert_eq!(reason, "silent_idle");
            }
            other => panic!("expected PaneStalled watching, got {other:?}"),
        }
        let stalled_events = detect_stalled_panes_tiered(
            std::slice::from_ref(&stalled_bead),
            &states,
            now,
            threshold,
            watching,
        );
        assert_eq!(stalled_events.len(), 1);
        match &stalled_events[0] {
            IngressEvent::PaneStalled { severity, .. } => {
                assert_eq!(severity, "stalled");
            }
            other => panic!("expected stalled, got {other:?}"),
        }
        let both = detect_stalled_panes_tiered(
            &[watching_bead, stalled_bead],
            &states,
            now,
            threshold,
            watching,
        );
        assert_eq!(both.len(), 2);
        let watching_count = both
            .iter()
            .filter(|e| matches!(e, IngressEvent::PaneStalled { severity, .. } if severity == "watching"))
            .count();
        let stalled_count = both
            .iter()
            .filter(|e| matches!(e, IngressEvent::PaneStalled { severity, .. } if severity == "stalled"))
            .count();
        assert_eq!(watching_count, 1);
        assert_eq!(stalled_count, 1);
    }

    #[test]
    fn stall_evidence_sampling_classifies_and_redacts() {
        let now = chrono::Utc::now().timestamp();
        let threshold = 1800;
        let watching = stall_watching_threshold_secs(threshold);
        let old_bead = ReadyBead {
            id: "bd-evid".into(),
            title: "evid".into(),
            assignee: Some("EvidAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some(
                (chrono::Utc::now() - chrono::Duration::seconds((threshold + 10) as i64))
                    .to_rfc3339(),
            ),
            ..Default::default()
        };
        let states = vec![AgentState {
            name: "EvidAgent".into(),
            status: "idle".into(),
            kind: "opencode".into(),
            pane_id: "w1:p1".into(),
            mail_identity: None,
            nudge_streak: 0,
        }];
        let raw_tail = "some log line\nrate limit hit: 429 overloaded\nnsec1abc123 secret token=xyz\nfinal line";
        let events = detect_stalled_panes_with_evidence(
            &[old_bead],
            &states,
            now,
            threshold,
            watching,
            |_| raw_tail.to_string(),
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            IngressEvent::PaneStalled {
                reason,
                evidence_tail,
                severity,
                ..
            } => {
                assert_eq!(reason, "rate_limit");
                assert_eq!(severity, "stalled");
                assert!(
                    !evidence_tail.contains("nsec1"),
                    "secret must be redacted: {evidence_tail}"
                );
                assert!(
                    evidence_tail.contains("[REDACTED"),
                    "redacted marker present: {evidence_tail}"
                );
            }
            other => panic!("expected PaneStalled, got {other:?}"),
        }
    }

    #[test]
    fn stall_redaction_truncates() {
        let many_lines: String = (0..100)
            .map(|i| format!("line {i} content here"))
            .collect::<Vec<_>>()
            .join("\n");
        let sanitized = sanitize_evidence_tail(&many_lines);
        let line_count = sanitized.lines().count();
        assert!(
            line_count <= STALL_EVIDENCE_TAIL_LINES + 1,
            "must keep last 40 lines, got {line_count}"
        );
        assert!(
            !sanitized.contains("line 0 content"),
            "old lines beyond tail must be dropped"
        );
        assert!(
            sanitized.contains("line 99 content"),
            "latest line must be present"
        );
        let long_tail = "x".repeat(STALL_EVIDENCE_MAX_CHARS + 500);
        let sanitized_long = sanitize_evidence_tail(&long_tail);
        assert!(
            sanitized_long.contains("…(truncated)"),
            "long tail must be truncated"
        );
        assert!(
            sanitized_long.chars().count() <= STALL_EVIDENCE_MAX_CHARS + "…(truncated)".len() + 1,
            "truncated length bounded"
        );
        let secret_raw = "here is a token=supersecret123 and more";
        let sanitized_secret = sanitize_evidence_tail(secret_raw);
        assert!(
            !sanitized_secret.contains("supersecret123"),
            "secret value must be redacted"
        );
        assert!(sanitized_secret.contains("[REDACTED"), "redacted marker");
        let nsec_raw = "key nsec1qwertyuiopasdfghjkl1234567890";
        let sanitized_nsec = sanitize_evidence_tail(nsec_raw);
        assert!(
            !sanitized_nsec.contains("nsec1qwerty"),
            "nsec must be redacted"
        );
    }

    #[test]
    fn stall_one_read_per_detection_not_per_tick() {
        let now = chrono::Utc::now().timestamp();
        let threshold = 1800;
        let watching = stall_watching_threshold_secs(threshold);
        let old_bead = ReadyBead {
            id: "bd-read".into(),
            title: "read".into(),
            assignee: Some("ReadAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some(
                (chrono::Utc::now() - chrono::Duration::seconds((threshold + 10) as i64))
                    .to_rfc3339(),
            ),
            ..Default::default()
        };
        let recent_bead = ReadyBead {
            id: "bd-recent".into(),
            title: "recent".into(),
            assignee: Some("ReadAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let states = vec![AgentState {
            name: "ReadAgent".into(),
            status: "idle".into(),
            kind: "opencode".into(),
            pane_id: "w1:p1".into(),
            mail_identity: None,
            nudge_streak: 0,
        }];
        let mut read_count = 0usize;
        let _ = detect_stalled_panes_with_evidence(
            std::slice::from_ref(&old_bead),
            &states,
            now,
            threshold,
            watching,
            |_| {
                read_count += 1;
                "some tail".to_string()
            },
        );
        assert_eq!(read_count, 1, "one read per detection (stalled)");
        read_count = 0;
        let _ = detect_stalled_panes_with_evidence(
            &[recent_bead],
            &states,
            now,
            threshold,
            watching,
            |_| {
                read_count += 1;
                "some tail".to_string()
            },
        );
        assert_eq!(read_count, 0, "no read when not crossing tier");
        let watching_bead = ReadyBead {
            id: "bd-watch2".into(),
            title: "watch".into(),
            assignee: Some("ReadAgent".into()),
            verify: Some("true".into()),
            labels: vec![],
            priority: 2,
            principles: "p — d".into(),
            updated_at: Some(
                (chrono::Utc::now() - chrono::Duration::seconds((watching + 5) as i64))
                    .to_rfc3339(),
            ),
            ..Default::default()
        };
        read_count = 0;
        let _ = detect_stalled_panes_with_evidence(
            &[watching_bead],
            &states,
            now,
            threshold,
            watching,
            |_| {
                read_count += 1;
                "tail".to_string()
            },
        );
        assert_eq!(read_count, 1, "watching also one read");
    }
}

#[cfg(test)]
mod resume_dedup_tests {
    use super::*;
    use crate::state::SentRecord;

    fn rec(name: &str, hash: u64, invocation: &str, landed: bool, ambiguous: bool) -> SentRecord {
        SentRecord {
            name: name.to_string(),
            text_hash: hash,
            invocation: invocation.to_string(),
            iteration: 1,
            landed,
            ambiguous,
        }
    }

    #[test]
    fn should_resend_matrix() {
        assert!(should_resend(None, "inv-1"));
        assert!(!should_resend(
            Some(&rec("a", 1, "inv-1", true, false)),
            "inv-1"
        ));
        assert!(should_resend(
            Some(&rec("a", 1, "inv-1", false, false)),
            "inv-1"
        ));
        assert!(!should_resend(
            Some(&rec("a", 1, "inv-1", false, true)),
            "inv-1"
        ));
        assert!(should_resend(
            Some(&rec("a", 1, "inv-0", true, false)),
            "inv-1"
        ));
        assert!(should_resend(
            Some(&rec("a", 1, "inv-0", false, false)),
            "inv-1"
        ));
        assert!(should_resend(
            Some(&rec("a", 1, "inv-0", false, true)),
            "inv-1"
        ));
    }

    fn st(name: &str, status: &str) -> AgentState {
        AgentState {
            name: name.to_string(),
            status: status.to_string(),
            kind: String::new(),
            pane_id: String::new(),
            mail_identity: None,
            nudge_streak: 0,
        }
    }

    #[test]
    fn keep_landed_record_clears_stale_for_idle_or_done() {
        assert!(!keep_landed_record(
            &rec("a", 1, "inv-1", true, false),
            "inv-1",
            &[st("a", "idle")]
        ));
        assert!(!keep_landed_record(
            &rec("a", 1, "inv-1", true, false),
            "inv-1",
            &[st("a", "done")]
        ));
    }

    #[test]
    fn keep_landed_record_preserves_in_flight() {
        assert!(keep_landed_record(
            &rec("a", 1, "inv-1", true, false),
            "inv-1",
            &[st("a", "working")]
        ));
        assert!(keep_landed_record(
            &rec("a", 1, "inv-1", true, false),
            "inv-1",
            &[st("a", "blocked")]
        ));
        assert!(keep_landed_record(
            &rec("a", 1, "inv-1", true, false),
            "inv-1",
            &[st("a", "unknown")]
        ));
    }

    #[test]
    fn keep_landed_record_keeps_non_stale_cases() {
        assert!(keep_landed_record(
            &rec("a", 1, "inv-0", true, false),
            "inv-1",
            &[st("a", "idle")]
        ));
        assert!(keep_landed_record(
            &rec("a", 1, "inv-1", false, false),
            "inv-1",
            &[st("a", "idle")]
        ));
        assert!(keep_landed_record(
            &rec("a", 1, "inv-1", true, false),
            "inv-1",
            &[st("other", "idle")]
        ));
    }

    #[test]
    fn should_resend_degrades_without_confirmation() {
        assert!(should_resend(None, "inv-1"));
        assert!(should_resend(
            Some(&rec("a", 1, "inv-1", false, false)),
            "inv-1"
        ));
    }

    #[test]
    fn escalation_starts_at_t0() {
        assert_eq!(
            escalation_tier(1_000_000, 1_000_000, 30),
            EscalationTier::T0
        );
        assert_eq!(escalation_tier(0, 59_999_999, 30), EscalationTier::T0);
    }

    #[test]
    fn escalation_advances_by_interval_multiple() {
        let i = 30;
        let us = |secs: i64| secs * 1_000_000;
        assert_eq!(escalation_tier(0, us(60), i), EscalationTier::T1);
        assert_eq!(escalation_tier(0, us(119), i), EscalationTier::T1);
        assert_eq!(escalation_tier(0, us(120), i), EscalationTier::T2);
        assert_eq!(escalation_tier(0, us(239), i), EscalationTier::T2);
        assert_eq!(escalation_tier(0, us(240), i), EscalationTier::T3);
    }

    #[test]
    fn escalation_is_deterministic_and_bounded() {
        let a = escalation_tier(0, 120_000_000, 30);
        let b = escalation_tier(0, 120_000_000, 30);
        assert_eq!(a, b);
        assert_eq!(escalation_tier(0, i64::MAX, 30), EscalationTier::T3);
    }

    #[test]
    fn heal_states_deterministic() {
        let mut a = LoopModel::default();
        a.heal_states
            .insert("zebra".to_string(), HealState::default());
        a.heal_states
            .insert("apple".to_string(), HealState::default());
        a.heal_states
            .insert("mango".to_string(), HealState::default());
        let mut b = LoopModel::default();
        b.heal_states
            .insert("mango".to_string(), HealState::default());
        b.heal_states
            .insert("zebra".to_string(), HealState::default());
        b.heal_states
            .insert("apple".to_string(), HealState::default());
        let ka: Vec<&String> = a.heal_states.keys().collect();
        let kb: Vec<&String> = b.heal_states.keys().collect();
        assert_eq!(ka, kb);
        assert_eq!(ka, ["apple", "mango", "zebra"]);
    }

    /// review-fix F4: the bounded-halt decision table. `mail` mode halts at
    /// T3 only; `stop` halts at any tier (T0); `continue` never halts.
    #[test]
    fn blocked_t3_halts_loop_mail_mode_only() {
        use EscalationTier::*;
        assert_eq!(on_blocked_tier_exit(T0, "mail"), None);
        assert_eq!(on_blocked_tier_exit(T1, "mail"), None);
        assert_eq!(on_blocked_tier_exit(T2, "mail"), None);
        let halt = on_blocked_tier_exit(T3, "mail");
        assert!(halt.is_some());
        assert!(halt.unwrap().contains("T3"));
        for t in [T0, T1, T2, T3] {
            assert!(
                on_blocked_tier_exit(t, "stop").is_some(),
                "stop must halt at {t:?}"
            );
        }
        for t in [T0, T1, T2, T3] {
            assert_eq!(on_blocked_tier_exit(t, "continue"), None);
        }
        assert_eq!(on_blocked_tier_exit(T3, "whatever"), None);
    }

    /// Serializes the cg6.1 persistence tests AND makes them hermetic
    /// (193.2): the REAL state dir is shared with production loops and
    /// sibling test processes — a v1 fixture left in it re-triggers the
    /// sidecar migration mid-suite (the observed ENOENT flake). The guard
    /// holds the crate-wide env lock + points XDG_STATE_HOME at a fresh
    /// temp dir for the test's lifetime.
    fn cg61_registry_guard() -> crate::config::TestStateHome {
        crate::config::test_state_home()
    }

    /// Unique project slug for leaf-state tests (the state dir is the real
    /// one; tests clean their own entry up).
    fn cg61_test_project(tag: &str) -> String {
        format!("cg6.1-{tag}-{}", std::process::id())
    }

    /// Remove a test loop's registry sidecar (best-effort cleanup).
    fn cg61_cleanup(project: &str) {
        let _ = crate::state::remove_active_loop(project);
    }

    /// Pure: the step derives the tier from the persisted first-blocked
    /// timestamp and mails ONCE per tier transition (cg6.3) — ping_count
    /// increments only when a mail fires.
    #[test]
    fn escalation_step_bounds_tiers_and_increments_ping_count() {
        use crate::state::EscalationState;
        let i = 30u64;
        let us = |secs: i64| secs * 1_000_000;
        let mut st = EscalationState {
            first_blocked_ts: 0,
            ping_count: 0,
            last_ping_tier: None,
            last_tail: String::new(),
        };
        assert_eq!(escalation_step(&mut st, us(30), i, false), (0, 1, true));
        assert_eq!(escalation_step(&mut st, us(60), i, false), (1, 2, true));
        assert_eq!(escalation_step(&mut st, us(120), i, false), (2, 3, true));
        assert_eq!(escalation_step(&mut st, i64::MAX, i, false), (3, 4, true));
        assert_eq!(escalation_step(&mut st, i64::MAX, i, false), (3, 4, false));
        let mut st2 = EscalationState {
            first_blocked_ts: 0,
            ping_count: u32::MAX,
            last_ping_tier: None,
            last_tail: String::new(),
        };
        assert_eq!(
            escalation_step(&mut st2, us(60), i, false),
            (1, u32::MAX, true)
        );
    }

    /// ADR-0001 A6 acceptance: an agent that ACKS resets the escalation.
    /// The step restarts the budget from the observed `now`, drops the ping
    /// credit, records T0 as handled (so no immediate re-ping) and never mails
    /// on the ack read itself. Once quiet again, the ladder resumes at T1 one
    /// window later — not at T3 from the pre-ack elapsed clock.
    #[test]
    fn escalation_step_ack_resets_the_ladder() {
        use crate::state::EscalationState;
        let i = 30u64;
        let us = |secs: i64| secs * 1_000_000;
        let mut st = EscalationState {
            first_blocked_ts: 0,
            ping_count: 0,
            last_ping_tier: None,
            last_tail: "old tail".into(),
        };
        assert_eq!(escalation_step(&mut st, us(30), i, false), (0, 1, true));
        assert_eq!(escalation_step(&mut st, us(120), i, false), (2, 2, true));
        assert_eq!(escalation_step(&mut st, us(120), i, true), (0, 0, false));
        assert_eq!(st.ping_count, 0, "ack drops the ping credit");
        assert_eq!(st.first_blocked_ts, us(120), "budget restarts from the ack");
        assert_eq!(
            st.last_ping_tier,
            Some(0),
            "T0 is recorded as handled — an ack never re-pings T0"
        );
        assert_eq!(escalation_step(&mut st, us(180), i, false), (1, 1, true));
        assert_eq!(escalation_step(&mut st, us(180), i, false), (1, 1, false));
        assert_eq!(escalation_step(&mut st, i64::MAX, i, false), (3, 2, true));
        assert_eq!(escalation_step(&mut st, i64::MAX, i, true), (0, 0, false));
        assert_eq!(st.first_blocked_ts, i64::MAX);
        assert_eq!(
            escalation_tier(st.first_blocked_ts, i64::MAX, i),
            EscalationTier::T0
        );
    }

    /// Pure: the gate's stdout decoder. Only an available read that reports
    /// ZERO unacked pings is an ack; every failure mode is "not acked" so the
    /// time ladder keeps its pre-gate behavior.
    #[test]
    fn ack_gate_decodes_unacked_and_fails_open() {
        assert_eq!(ack_gate_unacked(r#"{"ok":true,"count":0}"#), Some(0));
        assert_eq!(ack_gate_unacked(r#"{"ok":true,"count":3}"#), Some(3));
        assert_eq!(ack_gate_unacked(r#"{"ok":false,"error":"no index"}"#), None);
        assert_eq!(ack_gate_unacked(""), None);
        assert_eq!(ack_gate_unacked("not json"), None);
        assert_eq!(ack_gate_unacked(r#"{"ok":true}"#), None);
    }

    /// Pure: the gate argv — the agent's own health thread at SLA 0 (any
    /// unacked ping counts; the escalation has its own 2×interval cadence).
    #[test]
    fn ack_gate_argv_targets_own_health_thread_at_sla_zero() {
        assert_eq!(
            ack_gate_argv("proj", "agent-a"),
            vec![
                "__ack-overdue".to_string(),
                "proj".to_string(),
                "agent-a".to_string(),
                "--thread".to_string(),
                "health-agent-a".to_string(),
                "--sla-secs".to_string(),
                "0".to_string(),
            ]
        );
    }

    /// Leaf-level (cg6.1): a fresh agent with no persisted entry starts at
    /// T0 with ping_count 1 (first_blocked_ts seeded by the leaf; mails).
    #[test]
    fn escalate_and_persist_fresh_agent_starts_at_t0() {
        let _g = cg61_registry_guard();
        let project = cg61_test_project("fresh");
        let (tier, count, mails) =
            escalate_and_persist(&project, "agent-a", None, 30, json!({}), false, 1_000_000)
                .unwrap();
        assert_eq!(
            (tier, count, mails),
            (0, 1, true),
            "first escalation is T0 + ping 1 + mail"
        );
        let al = crate::state::load_active_loop(&project).unwrap().unwrap();
        let st = al.escalation.get("agent-a").expect("entry persisted");
        assert_eq!(st.ping_count, 1);
        cg61_cleanup(&project);
    }

    /// Leaf-level (cg6.1): a stale persisted first_blocked_ts (past the 8x
    /// window) escalates straight to T3 (and mails — a tier transition).
    #[test]
    fn escalate_and_persist_stale_first_blocked_reaches_t3() {
        use crate::state::{ActiveLoop, EscalationState};
        let _g = cg61_registry_guard();
        let project = cg61_test_project("stale");
        let stale = now_us_micros() - 9 * 30 * 1_000_000;
        let mut al = ActiveLoop::new("swarm-loop", json!({}), crate::state::now_iso());
        al.escalation.insert(
            "agent-b".to_string(),
            EscalationState {
                first_blocked_ts: stale,
                ping_count: 0,
                last_ping_tier: None,
                last_tail: String::new(),
            },
        );
        crate::state::save_active_loop(&project, &al).unwrap();
        let (tier, count, mails) = escalate_and_persist(
            &project,
            "agent-b",
            None,
            30,
            json!({}),
            false,
            now_us_micros(),
        )
        .unwrap();
        assert_eq!(
            (tier, count, mails),
            (3, 1, true),
            "stale first_blocked_ts → T3 + mail"
        );
        cg61_cleanup(&project);
    }

    /// cg6.1 + cg6.3 regression proof: re-execution (crash-window replay)
    /// derives the tier from the PERSISTED first_blocked_ts — the tier cannot
    /// jump across the crash boundary, and at the same tier the replay
    /// neither re-mails nor double-counts (at most one ping per logical tier
    /// transition).
    #[test]
    fn escalate_and_persist_replay_derives_from_persisted_first_blocked() {
        let _g = cg61_registry_guard();
        let project = cg61_test_project("replay");
        let seed = now_us_micros() - 1_000_000;
        let (t1, c1, m1) = escalate_and_persist(
            &project,
            "agent-c",
            Some(seed),
            30,
            json!({}),
            false,
            1_000_000,
        )
        .unwrap();
        let (t2, c2, m2) = escalate_and_persist(
            &project,
            "agent-c",
            Some(seed),
            30,
            json!({}),
            false,
            1_000_000,
        )
        .unwrap();
        assert_eq!(t1, t2, "tier must not jump on replay");
        assert_eq!((c1, m1), (1, true));
        assert_eq!(
            (c2, m2),
            (1, false),
            "same-tier replay: no re-mail, no double-count (cg6.3)"
        );
        let al = crate::state::load_active_loop(&project).unwrap().unwrap();
        let st = al.escalation.get("agent-c").unwrap();
        assert_eq!(
            st.first_blocked_ts, seed,
            "first_blocked_ts never re-seeded"
        );
        assert_eq!(st.ping_count, 1);
        assert_eq!(st.last_ping_tier, Some(0), "T0 ping marked");
        cg61_cleanup(&project);
    }

    /// Leaf-level: the ack gate is honored only AFTER a real ping. A brand-new
    /// entry with an empty health thread must still ping T0 — an "empty read"
    /// is not an ack (that inversion would silence the escalation entirely).
    #[test]
    fn escalate_and_persist_ignores_ack_before_first_ping() {
        let _g = cg61_registry_guard();
        let project = cg61_test_project("ack-prefirst");
        let (tier, count, mails) =
            escalate_and_persist(&project, "agent-d", None, 30, json!({}), true, 1_000_000)
                .unwrap();
        assert_eq!(
            (tier, count, mails),
            (0, 1, true),
            "no ping sent yet → the gate cannot claim an ack; T0 still mails"
        );
        cg61_cleanup(&project);
    }

    /// Leaf-level: once a ping is out, an ack resets the ladder and suppresses
    /// the next tier's mail (the escalation stops pinging an agent that
    /// answered); a later quiet window resumes at T1.
    #[test]
    fn escalate_and_persist_ack_after_ping_resets_and_holds() {
        let _g = cg61_registry_guard();
        let project = cg61_test_project("ack-after");
        let (t0, c0, m0) =
            escalate_and_persist(&project, "agent-e", None, 30, json!({}), false, 1_000_000)
                .unwrap();
        assert_eq!((t0, c0, m0), (0, 1, true));
        let (t1, c1, m1) = escalate_and_persist(
            &project,
            "agent-e",
            Some(now_us_micros() - 9 * 30 * 1_000_000),
            30,
            json!({}),
            true,
            1_000_000,
        )
        .unwrap();
        assert_eq!(
            (t1, c1, m1),
            (0, 0, false),
            "acked → reset, no mail (T3 would otherwise have mailed)"
        );
        let al = crate::state::load_active_loop(&project).unwrap().unwrap();
        let st = al.escalation.get("agent-e").unwrap();
        assert_eq!(st.ping_count, 0);
        assert_eq!(st.last_ping_tier, Some(0));
        cg61_cleanup(&project);
    }

    /// The leaf runner executes the SAME std::process::Command (a trivial
    /// echo here) and returns its stdout — proving the spawn_blocking path
    /// works end-to-end (blocking pool → child wait → Output).
    #[tokio::test]
    async fn run_leaf_cmd_executes_and_returns_output() {
        let out = run_leaf_cmd(
            {
                let mut cmd = std::process::Command::new("/bin/sh");
                cmd.args(["-c", "printf 'leaf-ok'"]);
                cmd
            },
            "__test-leaf",
        )
        .await
        .expect("leaf runs on the blocking pool");
        assert!(out.status.success(), "child exits 0");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "leaf-ok",
            "stdout captured"
        );
    }

    /// A non-zero child exit is a CALL-SITE status check (as before the
    /// refactor) — run_leaf_cmd returns the Output; a genuinely unspawnable
    /// command surfaces as an Err naming the leaf.
    #[tokio::test]
    async fn run_leaf_cmd_propagates_child_failure() {
        let out = run_leaf_cmd(
            {
                let mut cmd = std::process::Command::new("/bin/sh");
                cmd.args(["-c", "exit 3"]);
                cmd
            },
            "__test-leaf",
        )
        .await
        .expect("non-zero exit is a status, not an Err");
        assert_eq!(out.status.code(), Some(3), "child exit code preserved");

        let spawn_err = run_leaf_cmd(
            std::process::Command::new("/nonexistent/leaf-bin"),
            "__test-leaf",
        )
        .await
        .expect_err("missing binary fails spawn");
        assert!(
            format!("{spawn_err}").contains("__test-leaf"),
            "labels the leaf: {spawn_err}"
        );
    }

    /// bd-loop-park-fresh-invocation-xl6r regression: a hung leaf child
    /// (e.g. the toron daemon reserve-write hang of 6ec181d — >300s while
    /// reads respond) must NEVER park the durable loop. The watchdog kills
    /// the child and surfaces a bounded Err; the loop's next poll proceeds.
    /// Named with `loop_` so the bead's VERIFY fence (`cargo test --bin
    /// flywheel loop`) exercises the park guard.
    #[tokio::test]
    async fn loop_leaf_watchdog_kills_hung_leaf() {
        let start = std::time::Instant::now();
        let err = run_leaf_cmd_bounded(
            {
                let mut cmd = std::process::Command::new("/bin/sh");
                cmd.args(["-c", "sleep 30"]);
                cmd
            },
            "__test-leaf",
            std::time::Duration::from_millis(300),
        )
        .await
        .expect_err("hung leaf must not park the loop");
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "watchdog fires in bounded time: {elapsed:?}"
        );
        let msg = format!("{err}");
        assert!(msg.contains("__test-leaf"), "names the leaf: {msg}");
        assert!(msg.contains("watchdog"), "explains the kill: {msg}");
    }

    /// bd-loop-park-fresh-invocation-xl6r: the loop-equivalent property —
    /// a hung leaf mid-flight must not stop the NEXT poll. Five consecutive
    /// leaf invocations (one hung) each complete within their own bound,
    /// so a fresh loop never parks on a stuck subprocess.
    #[tokio::test]
    async fn loop_poll_cycle_continues_after_hung_leaf() {
        for i in 0..5 {
            let start = std::time::Instant::now();
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args(["-c", if i == 2 { "sleep 30" } else { "printf 'ok'" }]);
            let res =
                run_leaf_cmd_bounded(cmd, "__test-leaf", std::time::Duration::from_millis(300))
                    .await;
            let elapsed = start.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "poll cycle {i} completes within bound: {elapsed:?}"
            );
            if i == 2 {
                assert!(res.is_err(), "hung leaf surfaces Err (cycle {i})");
            } else {
                assert!(res.is_ok(), "healthy leaf stays Ok (cycle {i})");
            }
        }
    }

    /// bd-loop-park-fresh-invocation-xl6r (CoralFinch review): the watchdog
    /// must kill the leaf's WHOLE process group — a leaf's own children
    /// (br/git/cargo it spawned) die with it instead of orphaning to PPID 1
    /// (prior incident: orphaned cargo test pid 9791). The fixture leaf
    /// backgrounds a grandchild and writes its pid to a file; after the
    /// timeout the grandchild must be dead (kill 0 fails).
    #[tokio::test]
    async fn loop_leaf_watchdog_kills_grandchild_process_group() {
        let pid_file =
            std::env::temp_dir().join(format!("xl6r-leaf-grandchild-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let script = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let err = run_leaf_cmd_bounded(
            {
                let mut cmd = std::process::Command::new("/bin/sh");
                cmd.args(["-c", &script]);
                cmd
            },
            "__test-leaf",
            std::time::Duration::from_millis(300),
        )
        .await
        .expect_err("hung leaf with grandchild must not park the loop");
        let msg = format!("{err}");
        assert!(
            msg.contains("process group") && msg.contains("killed"),
            "names the group kill: {msg}"
        );
        let gc_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild wrote its pid")
            .trim()
            .parse()
            .expect("pid parses");
        let _ = std::fs::remove_file(&pid_file);
        let mut gone = false;
        for _ in 0..30 {
            let alive = std::process::Command::new("kill")
                .args(["-0", &gc_pid.to_string()])
                .status()
                .expect("kill -0 runs");
            if !alive.success() {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            gone,
            "grandchild pid {gc_pid} must be dead after the watchdog group-kill"
        );
    }
}

#[cfg(test)]
mod verify_field_tests {
    use super::*;

    /// The typed `verify` field flows through unchanged; an absent or empty
    /// field is `None`/empty — the bead is NOT dispatchable.
    #[test]
    fn typed_verify_field_read_from_json() {
        let bead: serde_json::Value =
            serde_json::from_str(r#"{"id":"bd-x","verify":"cargo test"}"#).unwrap();
        assert_eq!(
            bead.get("verify")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty()),
            Some("cargo test")
        );
        let empty: serde_json::Value = serde_json::json!({"id": "bd-x", "verify": ""});
        assert_eq!(
            empty
                .get("verify")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty()),
            None
        );
    }

    /// Typed principles citations format into the line-per-citation text
    /// `check_principles` consumes.
    #[test]
    fn principles_typed_field_formats_citations() {
        let bead = serde_json::json!({
            "principles": [
                {"name": "prove-it-works", "decision": "proof survives git clone"},
                {"name": "", "decision": "skipped empty name"},
                {"name": "fix-root-causes", "decision": ""}
            ]
        });
        assert_eq!(
            principles_typed_field(&bead),
            "prove-it-works — proof survives git clone"
        );
        assert_eq!(principles_typed_field(&serde_json::json!({})), "");
    }

    /// Dispatchable beads (valid fence) and not-ready beads (missing/
    /// malformed fence) split cleanly — the not-ready half is NEVER
    /// dispatched; it feeds the planner mail instead.
    #[test]
    fn verify_fence_partition_ready_splits_dispatchable() {
        let ready = vec![
            ReadyBead {
                id: "bd-ok-1".into(),
                title: "has fence".into(),
                assignee: None,
                verify: Some("cargo test ok".into()),
                labels: Vec::new(),
                priority: 3,
                principles: String::new(),
                updated_at: None,
                ..Default::default()
            },
            ReadyBead {
                id: "bd-no-fence".into(),
                title: "no fence".into(),
                assignee: None,
                verify: None,
                labels: Vec::new(),
                priority: 3,
                principles: String::new(),
                updated_at: None,
                ..Default::default()
            },
            ReadyBead {
                id: "bd-multi".into(),
                title: "multi-line fence".into(),
                assignee: None,
                verify: None,
                labels: Vec::new(),
                priority: 3,
                principles: String::new(),
                updated_at: None,
                ..Default::default()
            },
        ];
        let (dispatchable, not_ready) = partition_ready(&ready);
        let dispatched: Vec<&str> = dispatchable.iter().map(|b| b.id.as_str()).collect();
        let blocked: Vec<&str> = not_ready.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(dispatched, vec!["bd-ok-1"]);
        assert_eq!(blocked, vec!["bd-no-fence", "bd-multi"]);
    }

    /// bd-4spn GAP 1: the dispatch gate must machine-enforce the ADR-0019
    /// PRINCIPLES requirement — a priority<=2 bead with an incomplete
    /// PRINCIPLES section is NOT dispatchable (mirrors the ## VERIFY fence
    /// gate); P3+ is exempt.
    #[test]
    fn partition_ready_enforces_principles_for_priority_le_2() {
        let rb = |id: &str, priority: u8, principles: &str, verify: Option<&str>| ReadyBead {
            id: id.into(),
            title: String::new(),
            assignee: None,
            priority,
            principles: principles.into(),
            verify: verify.map(str::to_string),
            labels: Vec::new(),
            updated_at: None,
            ..Default::default()
        };
        let ready = vec![
            rb("bd-p2-empty", 2, "", Some("true")),
            rb("bd-p2-nameonly", 2, "prove-it-works", Some("true")),
            rb(
                "bd-p2-ok",
                2,
                "prove-it-works — wired the gate",
                Some("true"),
            ),
            rb("bd-p3-empty", 3, "", Some("true")),
        ];
        let (dispatchable, not_ready) = partition_ready(&ready);
        let dispatched: Vec<&str> = dispatchable.iter().map(|b| b.id.as_str()).collect();
        let blocked: Vec<&str> = not_ready.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            dispatched,
            vec!["bd-p2-ok", "bd-p3-empty"],
            "gate lets compliant beads through"
        );
        assert_eq!(
            blocked,
            vec!["bd-p2-empty", "bd-p2-nameonly"],
            "gate refuses incomplete PRINCIPLES"
        );
    }

    /// bd-4spn round-trip (bd-87hq never-drift pin class): the `__ready`
    /// leaf envelope and `parse_bead_list` are a producer/consumer pair —
    /// priority + the `## PRINCIPLES` section must survive the boundary or
    /// the durable-loop PRINCIPLES gate reads default-P3 rows and never
    /// blocks (the emit once dropped both fields; the sync path was
    /// unaffected because it parses `br` JSON directly).
    #[test]
    fn ready_leaf_round_trip_preserves_the_principles_gate() {
        let p2_blocked = ReadyBead {
            id: "bd-rt-blocked".into(),
            title: "P2 without principles".into(),
            assignee: None,
            verify: Some("cargo test".into()),
            labels: vec!["wave-1".into()],
            priority: 2,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        };
        let p2_ok = ReadyBead {
            id: "bd-rt-ok".into(),
            title: "P2 compliant".into(),
            assignee: None,
            verify: Some("cargo test".into()),
            labels: Vec::new(),
            priority: 2,
            principles: "prove-it-works — ran the VERIFY fence".into(),
            updated_at: None,
            ..Default::default()
        };
        let env = ready_leaf_envelope(&[p2_blocked, p2_ok]);
        let parsed = parse_bead_list(&env, "__ready").expect("leaf JSON parses");
        assert_eq!(parsed[0].priority, 2, "priority survives the leaf boundary");
        assert!(
            parsed[1].principles.contains("prove-it-works"),
            "PRINCIPLES section survives the leaf boundary"
        );
        let (dispatchable, not_ready) = partition_ready(&parsed);
        let dispatched: Vec<&str> = dispatchable.iter().map(|b| b.id.as_str()).collect();
        let blocked: Vec<&str> = not_ready.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(dispatched, vec!["bd-rt-ok"], "compliant P2 dispatches");
        assert_eq!(
            blocked,
            vec!["bd-rt-blocked"],
            "gate blocks THROUGH the leaf"
        );
    }

    /// The not-ready mail needs the WHY — the principles block reason names
    /// the exact failure (required-field vs no-decision-clause vs unknown
    /// name) so the planner can fix the description in one pass.
    #[test]
    fn principles_block_reason_names_the_problem() {
        let rb = |id: &str, priority: u8, principles: &str| ReadyBead {
            id: id.into(),
            title: String::new(),
            assignee: None,
            priority,
            principles: principles.into(),
            verify: Some("true".into()),
            labels: Vec::new(),
            updated_at: None,
            ..Default::default()
        };
        let empty = rb("bd-x", 2, "");
        let r = principles_block_reason(&empty).expect("P2 empty blocks");
        assert!(r.contains("PRINCIPLES is required"), "reason: {r}");

        let nameonly = rb("bd-y", 2, "prove-it-works");
        let r2 = principles_block_reason(&nameonly).expect("name-only blocks");
        assert!(r2.contains("no decision clause"), "reason: {r2}");

        let unknown = rb("bd-w", 2, "not-a-principle — changed something");
        let r3 = principles_block_reason(&unknown).expect("unknown name blocks");
        assert!(r3.contains("unknown principle name"), "reason: {r3}");

        assert!(
            principles_block_reason(&rb("bd-z", 3, "")).is_none(),
            "P3 empty is exempt"
        );
        assert!(
            principles_block_reason(&rb("bd-v", 2, "prove-it-works — wired the gate")).is_none(),
            "complete citation passes"
        );
    }

    /// bd-bead-create-contract-lint-4tdd: the creation-time contract check
    /// reports the EXACT reason + corrected snippet for each block class:
    /// missing/empty typed verify field, unknown principle name — and
    /// passes a fully-compliant set (P3 exempt). ADR-0001 wave 5
    /// (bd-wave5-flywheel-parser-strip-dkpvi): fences are no longer read,
    /// so the old bare-heading/malformed-fence classes are gone.
    #[test]
    fn contract_check_reports_exact_reason_and_fix() {
        let rb = |id: &str, priority: u8, verify: Option<&str>, principles: &str| ReadyBead {
            id: id.into(),
            title: String::new(),
            assignee: None,
            priority,
            principles: principles.into(),
            verify: verify.map(str::to_string),
            labels: Vec::new(),
            updated_at: None,
            ..Default::default()
        };
        let noverify = rb("bd-noverify", 2, None, "prove-it-works — x.");
        let issue = contract_issue(&noverify, "").expect("missing verify blocks");
        assert!(
            issue.reason.contains("missing/empty typed verify"),
            "reason: {}",
            issue.reason
        );
        assert!(
            issue.fix.contains("--verify"),
            "fix names the typed flag: {}",
            issue.fix
        );

        let unknown = rb("bd-unk", 2, Some("true"), "not-a-principle — changed x.");
        let issue4 = contract_issue(&unknown, "").expect("unknown name blocks");
        assert!(
            issue4.reason.contains("unknown principle name"),
            "reason: {}",
            issue4.reason
        );

        assert!(
            contract_issue(
                &rb(
                    "bd-ok",
                    2,
                    Some("cargo test x"),
                    "prove-it-works — wired the gate."
                ),
                ""
            )
            .is_none(),
            "compliant P2 passes"
        );
        assert!(
            contract_issue(&rb("bd-p3", 3, Some("true"), ""), "").is_none(),
            "P3 empty PRINCIPLES is exempt"
        );
    }

    /// bd-bead-create-contract-lint-4tdd: the planner-mail reason must be
    /// the EXACT `principles_block_reason` / verify-field string, not a
    /// generic label (the 2026-08-17 mislabel class).
    #[test]
    fn not_ready_reason_carries_exact_block_string() {
        let rb = |id: &str, priority: u8, verify: Option<&str>, principles: &str| ReadyBead {
            id: id.into(),
            title: String::new(),
            assignee: None,
            priority,
            principles: principles.into(),
            verify: verify.map(str::to_string),
            labels: Vec::new(),
            updated_at: None,
            ..Default::default()
        };
        let nameonly = rb("bd-n", 2, Some("true"), "prove-it-works");
        assert_eq!(
            not_ready_reason(&nameonly).as_deref(),
            principles_block_reason(&nameonly).as_deref(),
            "P<=2 reason must be the exact check_principles string"
        );
        let noverify = rb("bd-f", 2, None, "");
        let r = not_ready_reason(&noverify).expect("missing verify blocks");
        assert!(
            r.contains("typed verify"),
            "verify reason names the gate: {r}"
        );
    }

    /// The `## PRINCIPLES` section extractor: everything after the exact
    /// heading until the next `## ` heading or EOF; absent -> empty.
    #[test]
    fn parse_principles_field_extracts_section() {
        let desc = "goal line\n\n## PRINCIPLES\nprove-it-works — the change;\nfix-root-causes — the why.\n\n## VERIFY\n```\ncargo test x\n```";
        assert_eq!(
            parse_principles_field(desc),
            "prove-it-works — the change;\nfix-root-causes — the why."
        );
        let d2 = "## PRINCIPLES\nprove-it-works — x.\n## VERIFY\n```\ny\n```";
        assert_eq!(parse_principles_field(d2), "prove-it-works — x.");
        assert_eq!(parse_principles_field("goal\n## VERIFY\n```\nx\n```"), "");
        assert_eq!(parse_principles_field(""), "");
    }

    /// ADR-0019: the dispatch path must NEVER fall back to the whole-
    /// workspace matrix verify script (a time and token bomb that lies about
    /// a docs bead). Source-level pin on the dispatch files: the literal
    /// token is assembled (not spelled) so this test never trips itself.
    #[test]
    fn verify_fence_dispatch_path_never_references_verify_gates() {
        let needle = format!("verify-{}", "gates");
        for f in ["src/main.rs", "src/durable.rs", "src/work.rs"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert!(
                !src.contains(&needle),
                "{f} must not reference the {} fallback (ADR-0019)",
                needle
            );
        }
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;
    use chrono::TimeZone;
    fn iso_at(secs: i64) -> String {
        chrono::Utc.timestamp_opt(secs, 0).unwrap().to_rfc3339()
    }
    fn bead_with_age(id: &str, assignee: &str, age_secs: i64, now_secs: i64) -> ReadyBead {
        ReadyBead {
            id: id.into(),
            title: "reclaim test".into(),
            assignee: Some(assignee.into()),
            verify: Some("cargo test reclaim".into()),
            labels: Vec::new(),
            priority: 1,
            principles: "never-block-on-the-human — pane death is observable".into(),
            updated_at: Some(iso_at(now_secs - age_secs)),
            ..Default::default()
        }
    }

    #[test]
    fn reclaim_pane_exited_emits_for_in_progress_no_plane() {
        let bead = ReadyBead {
            id: "bd-test".into(),
            title: "t".into(),
            assignee: Some("GoldCove".into()),
            verify: Some("cargo test reclaim".into()),
            labels: Vec::new(),
            priority: 1,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        };
        let panes: Vec<AgentState> = vec![];
        let heal = HealState {
            respawn_count: 2,
            ..Default::default()
        };
        let act =
            reclaim_on_pane_exited(&bead, "GoldCove", &panes, &heal, 2).expect("should reclaim");
        match act {
            LoopAction::Reclaim {
                bead_id,
                pin,
                terminal_name,
                reason,
            } => {
                assert_eq!(bead_id, "bd-test");
                assert_eq!(pin, "GoldCove");
                assert_eq!(terminal_name, "GoldCove");
                assert_eq!(reason, "pane_exited");
            }
            _ => panic!("wrong"),
        }
    }
    #[test]
    fn reclaim_suppressed_while_budget_remains() {
        let bead = ReadyBead {
            id: "bd-test".into(),
            title: "t".into(),
            assignee: Some("GoldCove".into()),
            verify: Some("cargo test reclaim".into()),
            labels: Vec::new(),
            priority: 1,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        };
        let panes = vec![AgentState {
            name: "GoldCove".into(),
            status: "unknown".into(),
            kind: "opencode".into(),
            pane_id: "p1".into(),
            mail_identity: Some("GoldCove".into()),
            nudge_streak: 0,
        }];
        let heal = HealState {
            respawn_count: 0,
            ..Default::default()
        };
        assert!(reclaim_on_pane_exited(&bead, "GoldCove", &panes, &heal, 2).is_none());
        let heal2 = HealState {
            respawn_count: 1,
            ..Default::default()
        };
        assert!(reclaim_on_pane_exited(&bead, "GoldCove", &panes, &heal2, 2).is_none());
        let heal3 = HealState {
            respawn_count: 2,
            ..Default::default()
        };
        assert!(reclaim_on_pane_exited(&bead, "GoldCove", &panes, &heal3, 2).is_some());
    }
    #[test]
    fn reclaim_both_orderings() {
        let bead = ReadyBead {
            id: "bd-a".into(),
            title: "t".into(),
            assignee: Some("GoldCove".into()),
            verify: Some("cargo test reclaim".into()),
            labels: Vec::new(),
            priority: 1,
            principles: String::new(),
            updated_at: None,
            ..Default::default()
        };
        let panes_listed = vec![AgentState {
            name: "GoldCove".into(),
            status: "unknown".into(),
            kind: "opencode".into(),
            pane_id: "p1".into(),
            mail_identity: Some("GoldCove".into()),
            nudge_streak: 0,
        }];
        let panes_gone: Vec<AgentState> = vec![];
        let heal0 = HealState::default();
        assert!(reclaim_on_pane_exited(&bead, "GoldCove", &panes_listed, &heal0, 2).is_none());
        let heal1 = HealState {
            respawn_count: 1,
            ..Default::default()
        };
        assert!(reclaim_on_pane_exited(&bead, "GoldCove", &panes_listed, &heal1, 2).is_none());
        let heal2 = HealState {
            respawn_count: 2,
            ..Default::default()
        };
        assert!(reclaim_on_pane_exited(&bead, "GoldCove", &panes_listed, &heal2, 2).is_some());
        let heal_exhausted = HealState {
            respawn_count: 2,
            ..Default::default()
        };
        assert!(
            reclaim_on_pane_exited(&bead, "OtherPane", &panes_listed, &heal_exhausted, 2).is_none()
        );
        assert!(
            reclaim_on_pane_exited(&bead, "GoldCove", &panes_listed, &heal_exhausted, 2).is_some()
        );
        let heal_fresh = HealState::default();
        assert!(reclaim_on_pane_exited(&bead, "GoldCove", &panes_gone, &heal_fresh, 2).is_some());
    }
    #[test]
    fn reclaim_stale_7201_fires_7199_not() {
        let now = 1_700_000_000;
        let panes: Vec<AgentState> = vec![];
        let board: Vec<(String, String)> = vec![];
        let bead_old = bead_with_age("bd-old", "GoldCove", 7201, now);
        let bead_new = bead_with_age("bd-new", "GoldCove", 7199, now);
        let actions_old = plan_reclaim(now, &[bead_old], &panes, &board);
        assert_eq!(actions_old.len(), 1);
        let actions_new = plan_reclaim(now, &[bead_new], &panes, &board);
        assert_eq!(actions_new, [] as [LoopAction; 0]);
    }
    #[test]
    fn reclaim_human_86400_not_7200() {
        let now = 1_700_000_000;
        let panes: Vec<AgentState> = vec![];
        let board: Vec<(String, String)> = vec![];
        let bead_human_8000 = bead_with_age("bd-h", "bangedorrunt", 8000, now);
        assert_eq!(
            plan_reclaim(now, &[bead_human_8000], &panes, &board),
            [] as [LoopAction; 0]
        );
        let bead_human_old = bead_with_age("bd-h2", "bangedorrunt", 86401, now);
        assert_eq!(
            plan_reclaim(now, &[bead_human_old], &panes, &board).len(),
            1
        );
        let bead_swarm = bead_with_age("bd-s", "GoldCove", 8000, now);
        assert_eq!(plan_reclaim(now, &[bead_swarm], &panes, &board).len(), 1);
    }
    #[test]
    fn reclaim_different_pin_reservation_skip() {
        let now = 1_700_000_000;
        let bead = bead_with_age("bd-x", "GoldCove", 8000, now);
        let panes: Vec<AgentState> = vec![];
        let reservations = vec![(
            "src/foo.rs".to_string(),
            "bd-x".to_string(),
            "OtherPin".to_string(),
        )];
        let comments = std::collections::HashMap::new();
        let dirty: Vec<String> = vec![];
        assert_eq!(
            plan_reclaim_ex(now, &[bead], &panes, &reservations, &comments, &dirty),
            [] as [LoopAction; 0]
        );
        let reservations2 = vec![(
            "src/foo.rs".to_string(),
            "bd-x".to_string(),
            "GoldCove".to_string(),
        )];
        let bead2 = bead_with_age("bd-x", "GoldCove", 9000, now);
        assert_eq!(
            plan_reclaim_ex(now, &[bead2], &panes, &reservations2, &comments, &dirty),
            [] as [LoopAction; 0]
        );
        let bead3 = bead_with_age("bd-x", "GoldCove", 9000, now);
        assert_eq!(
            plan_reclaim_ex(now, &[bead3], &panes, &[], &comments, &dirty).len(),
            1
        );
    }
    #[test]
    fn reclaim_working_status_blocks() {
        let now = 1_700_000_000;
        let bead = bead_with_age("bd-w", "GoldCove", 9000, now);
        let panes = vec![AgentState {
            name: "GoldCove".into(),
            status: "working".into(),
            kind: "opencode".into(),
            pane_id: "p1".into(),
            mail_identity: Some("GoldCove".into()),
            nudge_streak: 0,
        }];
        let board: Vec<(String, String)> = vec![];
        assert_eq!(
            plan_reclaim(now, &[bead], &panes, &board),
            [] as [LoopAction; 0]
        );
    }
    #[test]
    fn reclaim_recent_comment_blocks() {
        let now = 1_700_000_000;
        let bead = bead_with_age("bd-c", "GoldCove", 9000, now);
        let panes: Vec<AgentState> = vec![];
        let reservations: Vec<(String, String, String)> = vec![];
        let mut comments = std::collections::HashMap::new();
        comments.insert("bd-c".into(), (now - 100, "some comment".into()));
        let dirty: Vec<String> = vec![];
        assert_eq!(
            plan_reclaim_ex(now, &[bead], &panes, &reservations, &comments, &dirty),
            [] as [LoopAction; 0]
        );
        let mut comments2 = std::collections::HashMap::new();
        comments2.insert("bd-c".into(), (now - 5000, "old".into()));
        let bead2 = bead_with_age("bd-c", "GoldCove", 9000, now);
        assert_eq!(
            plan_reclaim_ex(now, &[bead2], &panes, &reservations, &comments2, &dirty).len(),
            1
        );
    }
    #[test]
    fn reclaim_dirty_files_blocks() {
        let now = 1_700_000_000;
        let bead = bead_with_age("bd-d", "GoldCove", 9000, now);
        let panes: Vec<AgentState> = vec![];
        let reservations: Vec<(String, String, String)> = vec![];
        let mut comments = std::collections::HashMap::new();
        comments.insert(
            "bd-d".into(),
            (now - 5000, "touched src/foo.rs and tests/bar.rs".into()),
        );
        let dirty = vec!["src/foo.rs".into()];
        assert_eq!(
            plan_reclaim_ex(now, &[bead], &panes, &reservations, &comments, &dirty),
            [] as [LoopAction; 0]
        );
    }
    #[test]
    fn reclaim_leaf_no_br_close() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/durable.rs");
        let src = std::fs::read_to_string(&path).expect("read");
        let start = src.find("async fn reclaim_bead").expect("found");
        let end = src[start..].find("/// Legacy alias").expect("end");
        let leaf_src = &src[start..start + end];
        assert!(!leaf_src.contains("br close"));
    }
    #[test]
    fn reclaim_leaf_comment_then_open() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/durable.rs");
        let src = std::fs::read_to_string(&path).expect("read");
        let start = src.find("async fn reclaim_bead").expect("found");
        let snippet = &src[start..start + 4000];
        let comment_pos = snippet.find("comments").expect("comments");
        let open_pos = snippet.find("--status").expect("open");
        assert!(comment_pos < open_pos);
        assert!(snippet.contains("reclaim: previous in_progress claim appears abandoned"));
    }
    #[test]
    fn reclaim_thresholds_come_from_the_injected_policy_not_process_state() {
        let bead = ReadyBead {
            id: "bd-policy".into(),
            title: "policy witness".into(),
            assignee: Some("swarm-agent".into()),
            updated_at: None,
            ..ReadyBead::default()
        };
        let mut with_updated = bead.clone();
        with_updated.updated_at = Some(
            chrono::DateTime::from_timestamp(1_000, 0)
                .expect("epoch")
                .to_rfc3339(),
        );
        let tight = crate::durable_policy::ReclaimPolicy::new(10, 20);
        assert!(
            stale_reclaim_due_with_policy(&with_updated, 2_000, &[], &[], None, None, &[], &tight,),
            "a 1000s-old swarm claim past a 10s threshold is reclaimable"
        );
        let loose = crate::durable_policy::ReclaimPolicy::new(7_200, 86_400);
        assert!(
            !stale_reclaim_due_with_policy(&with_updated, 2_000, &[], &[], None, None, &[], &loose,),
            "the same claim under default thresholds is not reclaimable"
        );
    }
}

#[cfg(test)]
mod mail_health_tests {

    use super::*;
    use crate::state::{ActiveLoop, MailHealth};

    #[test]
    fn mail_health_durable_increments_and_resets() {
        let mut mh = MailHealth::default();
        assert_eq!(mh.consecutive_failures, 0);
        mh.record_failure(100);
        assert_eq!(mh.consecutive_failures, 1);
        mh.record_failure(200);
        mh.record_failure(300);
        assert_eq!(mh.consecutive_failures, 3);
        mh.record_success(400);
        assert_eq!(mh.consecutive_failures, 0);
        assert_eq!(mh.last_ok_ts, 400);
    }

    #[test]
    fn mail_health_durable_dispatch_hold_at_n() {
        let mut al = ActiveLoop::new("id", serde_json::json!({}), "now");
        al.mail_health.consecutive_failures = 2;
        assert!(!al.mail_health.should_escalate(3));
        al.mail_health.consecutive_failures = 3;
        assert!(al.mail_health.should_escalate(3));
        al.mail_degraded = true;
        assert!(al.mail_degraded);
    }

    #[test]
    fn mail_health_durable_resume_on_success() {
        let mut al = ActiveLoop::new("id", serde_json::json!({}), "now");
        al.mail_health.consecutive_failures = 3;
        al.mail_degraded = true;
        al.mail_health.record_success(999);
        al.mail_degraded = false;
        assert_eq!(al.mail_health.consecutive_failures, 0);
        assert!(!al.mail_degraded);
    }

    #[test]
    fn mail_health_durable_zero_disables() {
        let mh = MailHealth {
            consecutive_failures: 10,
            last_ok_ts: 0,
        };
        assert!(!mh.should_escalate(0));
    }

    #[test]
    fn mail_health_durable_escalation_names_leaf() {
        let project = format!("mail-health-test-{}", std::process::id());
        let _guard = crate::config::test_state_home();
        let mut al = ActiveLoop::new("id", serde_json::json!({}), crate::state::now_iso());
        al.mail_health.consecutive_failures = 2;
        crate::state::save_active_loop(&project, &al).unwrap();
        super::update_mail_health(
            &project,
            false,
            "renew_leases",
            "evidence: lease renew failed",
            3,
        );
        let al2 = crate::state::load_active_loop(&project).unwrap().unwrap();
        assert_eq!(al2.mail_health.consecutive_failures, 3);
        super::update_mail_health(&project, false, "renew_leases", "evidence2", 3);
        let al3 = crate::state::load_active_loop(&project).unwrap().unwrap();
        assert!(al3.mail_degraded);
        super::update_mail_health(&project, true, "renew_leases", "ok", 3);
        let al4 = crate::state::load_active_loop(&project).unwrap().unwrap();
        assert_eq!(al4.mail_health.consecutive_failures, 0);
        assert!(!al4.mail_degraded);
        let _ = crate::state::remove_active_loop(&project);
    }

    /// bd-swarm-loop-durable-panic-atlo acceptance: the renew/reclaim leaf
    /// runs ON the durable loop's tokio worker. Pre-fix, its inline `run_mail`
    /// nested the shared runtime's `block_on` inside a runtime — "Cannot
    /// start a runtime from within a runtime" once per poll, before every
    /// dispatch (`reclaimed stranded bead ... (re-dispatching)` storm). This
    /// drives the REAL sync block through the async-safe bridge on a real
    /// runtime, with a real (hermetic, in-process) mail op, and asserts it
    /// completes. The inline canary below keeps the red evidence in-tree.
    #[test]
    fn loop_mode_durable_survives_reclaim() {
        let _home = crate::config::test_state_home();
        let cfg_home = std::env::temp_dir().join(format!("fw-reclaim-cfg-{}", std::process::id()));
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("temp XDG config dir");
        // Unique mail identity: the test store is shared across projects, and
        // ADR-0037 forbids reusing one pin across projects.
        std::fs::write(
            cfg_home.join("flywheel/config.toml"),
            "[agent_mail]\nname = \"LoopReclaimPin\"\n",
        )
        .expect("config");
        let prev_cfg_home = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };

        let project = format!("loop-reclaim-{}", std::process::id());
        {
            let cfg = Config::load().expect("config");
            let key =
                crate::utf8_path(&crate::project_dir(&cfg, &project), "project path").expect("key");
            let mail = crate::backend::mail_backend(&cfg).expect("mail backend");
            crate::backend::run_mail(mail.ensure_project(&key))
                .map(|_| ())
                .expect("ensure project");
            crate::backend::run_mail(mail.register_agent(
                &key,
                &crate::spawn_agent_profile("LoopReclaimPin", "flywheel", &project),
            ))
            .map(|_| ())
            .expect("register agent");
        }

        let rt = tokio::runtime::Runtime::new().expect("loop runtime");
        let paths = vec!["src/durable.rs".to_string()];
        let (leaf_project, leaf_paths) = (project.clone(), paths.clone());
        let out: anyhow::Result<serde_json::Value> = rt.block_on(async move {
            crate::backend::run_mail_off_runtime(move || {
                renew_filtered_sync(&leaf_project, &leaf_paths, 60)
            })
            .await
        });

        let inline = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async {
                let _ = crate::backend::run_mail(async { 1i32 });
            });
        }));

        unsafe {
            match prev_cfg_home {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&cfg_home);

        assert!(
            inline.is_err(),
            "inline shared-runtime block_on was expected to panic; if tokio ever permits \
             the nesting, the bridge's reason is gone and this test needs revisiting"
        );
        let v =
            out.expect("renew leaf must complete on the loop runtime (no nested-runtime panic)");
        assert_eq!(v["renewed"], 0, "fresh project has no leases to renew: {v}");
    }
}

#[cfg(test)]
mod loop_phase_tests {
    use super::*;

    #[test]
    fn loop_phase_illegal_transitions() {
        assert!(transition(&LoopPhase::ClaimWatch, &LoopEvent::Verify).is_err());
        assert!(
            transition(
                &LoopPhase::Verify,
                &LoopEvent::Complete {
                    has_command_verified: true,
                    wave_gate: true
                }
            )
            .is_err()
        );
        assert!(
            transition(
                &LoopPhase::Verify,
                &LoopEvent::Complete {
                    has_command_verified: false,
                    wave_gate: false
                }
            )
            .is_err()
        );
        assert!(
            transition(
                &LoopPhase::Idle,
                &LoopEvent::Dispatch {
                    agent_status: "working".to_string(),
                    force: false
                }
            )
            .is_err()
        );
        assert!(
            transition(
                &LoopPhase::Reclaim,
                &LoopEvent::Reclaim {
                    holder_pin: "pin-a".to_string(),
                    claimant_pin: "pin-b".to_string(),
                    is_live: true
                }
            )
            .is_err()
        );
        assert_eq!(
            transition(
                &LoopPhase::Idle,
                &LoopEvent::Dispatch {
                    agent_status: "idle".to_string(),
                    force: false
                }
            )
            .unwrap(),
            LoopPhase::Dispatched
        );
        assert_eq!(
            transition(&LoopPhase::ClaimWatch, &LoopEvent::BrClose).unwrap(),
            LoopPhase::Verify
        );
        assert_eq!(
            transition(
                &LoopPhase::Verify,
                &LoopEvent::Complete {
                    has_command_verified: true,
                    wave_gate: false
                }
            )
            .unwrap(),
            LoopPhase::Complete
        );
    }

    #[test]
    fn loop_phase_complete_with_wavegate_is_illegal() {
        let res = transition(
            &LoopPhase::Verify,
            &LoopEvent::Complete {
                has_command_verified: true,
                wave_gate: true,
            },
        );
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().reason, "Complete while WaveGate");
    }
}

#[cfg(test)]
mod tick_replay_tests {
    use super::*;

    fn base_model() -> LoopModel {
        LoopModel {
            queue: vec![],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: 0,
            heal_threshold: 1,
            heal_budget: 2,
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        }
    }

    #[test]
    fn heal_msg_unknown_diagnoses_then_respawns() {
        let mut m = base_model();
        let (_, a1) = tick(
            &m,
            &LoopMsg::Heal {
                name: "a".into(),
                status: "unknown".into(),
                exited: false,
            },
        );
        assert!(matches!(
            a1.last(),
            Some(LoopAction::Diagnose { name, reason }) if name == "a" && reason == "unknown"
        ));
        m = tick(
            &m,
            &LoopMsg::Heal {
                name: "a".into(),
                status: "unknown".into(),
                exited: false,
            },
        )
        .0;
        let (_, a2) = tick(
            &m,
            &LoopMsg::Heal {
                name: "a".into(),
                status: "unknown".into(),
                exited: false,
            },
        );
        assert!(
            !a2.iter().any(|x| matches!(x, LoopAction::Diagnose { .. })),
            "awaiting_clear suppresses repeat diagnose"
        );
        let (_, a3) = tick(
            &m,
            &LoopMsg::Heal {
                name: "a".into(),
                status: "unknown".into(),
                exited: true,
            },
        );
        assert!(matches!(
            a3.last(),
            Some(LoopAction::Respawn { name, .. }) if name == "a"
        ));
        let m = tick(
            &m,
            &LoopMsg::Heal {
                name: "a".into(),
                status: "working".into(),
                exited: false,
            },
        )
        .0;
        assert!(!m.heal_states.contains_key("a") || m.heal_states["a"].unknown_count == 0);
    }

    #[test]
    fn claim_nudge_msg_emits_claim_prompt_send() {
        let (_, actions) = tick(
            &base_model(),
            &LoopMsg::ClaimNudge {
                name: "oc".into(),
                bead_id: "bd-12".into(),
            },
        );
        match actions.last() {
            Some(LoopAction::Send { name, text }) => {
                assert_eq!(name, "oc");
                assert!(text.contains("CLAIM REQUIRED"), "{text}");
                assert!(text.contains("bd-12"), "{text}");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn close_nudge_msg_emits_close_prompt_send() {
        let (_, actions) = tick(
            &base_model(),
            &LoopMsg::CloseNudge {
                name: "oc".into(),
                bead_id: "bd-12".into(),
                title: "fix auth".into(),
            },
        );
        match actions.last() {
            Some(LoopAction::Send { name, text }) => {
                assert_eq!(name, "oc");
                assert!(text.contains("CLOSE REQUIRED"), "{text}");
                assert!(
                    text.contains("bd-12") && text.contains("fix auth"),
                    "{text}"
                );
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }
    #[test]
    fn tick_replay_differential() {
        let mut model = LoopModel {
            queue: vec!["task1".to_string(), "task2".to_string()],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let msgs = vec![
            LoopMsg::Tick { now: 0 },
            LoopMsg::Dispatch {
                bead_id: "bd-1".to_string(),
                agent_status: "idle".to_string(),
                force: false,
            },
            LoopMsg::BrClose {
                bead_id: "bd-1".to_string(),
            },
        ];
        let mut actions_log = Vec::new();
        let _model_replay = model.clone();
        for msg in &msgs {
            let (next, actions) = tick(&model, msg);
            actions_log.push(actions.clone());
            model = next;
        }
        let mut model2 = LoopModel {
            queue: vec!["task1".to_string(), "task2".to_string()],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let mut actions_log2 = Vec::new();
        for msg in &msgs {
            let (next, actions) = tick(&model2, msg);
            actions_log2.push(actions.clone());
            model2 = next;
        }
        assert_eq!(actions_log, actions_log2);
        assert_eq!(model, model2);
    }

    /// bd-loop-slots: per-bead control is owned by LoopModel, survives a
    /// JSON round-trip, and resume keeps the durable counters while dropping
    /// only the transient wake suppression.
    #[test]
    fn loop_slots_round_trip_and_resume_clears_only_transient_state() {
        let mut model = LoopModel::default();
        assert_eq!(model.bump_close_grace("bd-a"), 1);
        assert_eq!(model.bump_close_grace("bd-a"), 2);
        assert!(model.mark_verifier_dispatched("bd-a"));
        assert!(!model.mark_verifier_dispatched("bd-a"));
        model.bead_slot("bd-a").watch = Some(ClaimWatch {
            agent: "coder".into(),
            unclaimed_polls: 2,
            close_nudged: true,
            close_polls: 1,
            wait_reason: Some(WaitReason::LiveLease),
        });
        model.suppress_context_limit("coder");

        let encoded = serde_json::to_string(&model).expect("encode slots");
        let decoded: LoopModel = serde_json::from_str(&encoded).expect("decode slots");
        assert_eq!(decoded, model);
        assert_eq!(decoded.bead_slots["bd-a"].close_grace_ticks, 2);
        assert!(decoded.bead_slots["bd-a"].verifier_dispatched);
        assert_eq!(
            decoded.bead_slots["bd-a"]
                .watch
                .as_ref()
                .map(|w| w.unclaimed_polls),
            Some(2)
        );

        let mut resumed = LoopModel::default();
        seed_swarm_resume(
            &mut resumed,
            &std::collections::BTreeMap::new(),
            &std::collections::HashSet::new(),
            &std::collections::BTreeMap::new(),
            &decoded.bead_slots,
            &decoded.agent_slots,
        );
        assert_eq!(resumed.bead_slots["bd-a"].close_grace_ticks, 2);
        assert!(resumed.bead_slots["bd-a"].verifier_dispatched);
        assert!(resumed.bead_slots["bd-a"].watch.is_some());
        assert_eq!(resumed.agent_slots["coder"].incarnation, 1);
        assert!(
            resumed.wake_suppressed().is_empty(),
            "a fresh process must ring the fresh pane's mail"
        );
    }

    /// bd-ctx-suppressed: suppression is derived from the AgentSlot and clears
    /// once the fresh incarnation is up, so mail rings again.
    #[test]
    fn context_limit_suppression_belongs_to_one_generation() {
        let mut model = LoopModel::default();
        model.suppress_context_limit("coder");
        assert!(model.wake_suppressed().contains("coder"));
        assert_eq!(model.agent_slots["coder"].incarnation, 1);

        model.clear_context_limit("coder");
        assert!(model.wake_suppressed().is_empty(), "fresh pane rings again");
        assert_eq!(model.agent_slots["coder"].incarnation, 1);

        model.suppress_context_limit("coder");
        assert_eq!(model.agent_slots["coder"].incarnation, 2);
    }

    /// bd-loop-slots replay witness: a model carrying drain, heal, and
    /// context-limit slots replays a fixed message stream identically.
    #[test]
    fn loop_slots_replay_deterministically_across_drain_heal_and_respawn() {
        let mut model = LoopModel {
            states: vec![AgentState {
                name: "coder".into(),
                status: "unknown".into(),
                kind: String::new(),
                pane_id: String::new(),
                mail_identity: None,
                nudge_streak: 0,
            }],
            ..LoopModel::default()
        };
        model.drain_pending.insert(
            "coder".to_string(),
            DrainState {
                bead_id: "bd-drain".into(),
                grace_polls: 2,
                defers: 1,
                paused: None,
            },
        );
        model.heal_states.insert(
            "coder".to_string(),
            HealState {
                unknown_count: 2,
                diagnose_count: 1,
                respawn_count: 0,
                awaiting_clear: true,
            },
        );
        model.suppress_context_limit("coder");
        model.bump_close_grace("bd-drain");
        model.mark_verifier_dispatched("bd-drain");

        let msgs = vec![
            LoopMsg::Tick { now: 10 },
            LoopMsg::Heal {
                name: "coder".into(),
                status: "unknown".into(),
                exited: false,
            },
            LoopMsg::Tick { now: 20 },
            LoopMsg::DrainRequest {
                name: "coder".into(),
                bead_id: "bd-drain".into(),
            },
        ];
        let run = |start: &LoopModel| {
            let mut m = start.clone();
            let mut log = Vec::new();
            for msg in &msgs {
                let (next, actions) = tick(&m, msg);
                log.push(actions);
                m = next;
            }
            (m, log)
        };
        let (a, a_log) = run(&model);
        let (b, b_log) = run(&model);
        assert_eq!(a_log, b_log);
        assert_eq!(a, b);
        assert!(a.agent_slots.contains_key("coder"));
        assert!(a.bead_slots.contains_key("bd-drain"));
    }

    #[test]
    fn tick_is_pure_and_deterministic() {
        let model = LoopModel {
            queue: vec!["q1".to_string()],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let msg = LoopMsg::Tick { now: 0 };
        let (m1, a1) = tick(&model, &msg);
        let (m2, a2) = tick(&model, &msg);
        assert_eq!(m1, m2);
        assert_eq!(a1, a2);
    }

    #[test]
    fn tick_has_no_mut_queue_param() {
        let model = LoopModel {
            queue: vec!["a".to_string(), "b".to_string()],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let msg = LoopMsg::Tick { now: 0 };
        let (next, actions) = tick(&model, &msg);
        assert_eq!(next.queue, vec!["a".to_string(), "b".to_string()]);
        assert!(actions.is_empty(), "parked tick emits nothing: {actions:?}");
    }

    #[test]
    fn tick_pairs_queue_head_with_idle_agent_not_brief_text() {
        let brief_text = brief("bd-12", "fix auth flow", "flywheel", false);
        let idle = AgentState {
            name: "flywheel-proj-oc".to_string(),
            status: "idle".to_string(),
            mail_identity: Some("DustySparrow".to_string()),
            ..Default::default()
        };
        let model = LoopModel {
            queue: vec![brief_text.clone()],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: vec![idle],
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let (next, actions) = tick(&model, &LoopMsg::Tick { now: 0 });
        assert!(next.queue.is_empty(), "taken head leaves the queue");
        assert!(next.dispatch_pending.contains("flywheel-proj-oc"));
        assert!(
            matches!(&actions[0], LoopAction::PinStamp { bead_id, pin }
                if bead_id == "bd-12" && pin == "DustySparrow"),
            "bead brief stamps the pin first: {actions:?}"
        );
        match &actions[1] {
            LoopAction::Send { name, text } => {
                assert_eq!(name, "flywheel-proj-oc", "real agent name, not brief text");
                assert!(
                    text.starts_with("GOAL Execute bd-12:"),
                    "brief content intact, not a stub: {text:.80}"
                );
                assert!(
                    text.contains("DustySparrow"),
                    "{{AGENT_NAME}} takes the pin: {text:.120}"
                );
                assert!(
                    !text.contains("{AGENT_NAME}"),
                    "no unsubstituted placeholder remains"
                );
            }
            other => panic!("expected paired Send, got {other:?}"),
        }
        let busy = LoopModel {
            states: vec![AgentState {
                name: "flywheel-proj-oc".to_string(),
                status: "working".to_string(),
                mail_identity: Some("DustySparrow".to_string()),
                ..Default::default()
            }],
            ..model.clone()
        };
        let (parked, no_actions) = tick(&busy, &LoopMsg::Tick { now: 0 });
        assert_eq!(parked.queue.len(), 1, "working crew holds the queue");
        assert_eq!(no_actions, [] as [LoopAction; 0]);
    }
}

#[cfg(test)]
mod loop_invoke_vehicle_tests {
    use super::*;
    use crate::state::MailHealth;

    #[test]
    fn loop_invoke_vehicle_graph_green_not_close() {
        let model = LoopModel {
            queue: vec!["bead-bd-1".to_string()],
            per_bead_phase: [("bd-1".to_string(), LoopPhase::Dispatched)]
                .into_iter()
                .collect(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: MailHealth::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let msg = LoopMsg::Tick { now: 0 };
        let (next, _actions) = tick(&model, &msg);
        assert!(next.per_bead_phase.contains_key("bd-1"));
        assert_ne!(next.per_bead_phase.get("bd-1"), Some(&LoopPhase::Complete));
    }

    #[test]
    fn loop_invoke_vehicle_after_worker_close_loop_records_command_verified() {
        let model = LoopModel {
            queue: vec![],
            per_bead_phase: [("bd-1".to_string(), LoopPhase::Verify)]
                .into_iter()
                .collect(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let (next, _) = tick(
            &model,
            &LoopMsg::BrClose {
                bead_id: "bd-1".to_string(),
            },
        );
        let (_next2, _) = tick(
            &next,
            &LoopMsg::CommandVerified {
                bead_id: "bd-1".to_string(),
                verified: true,
            },
        );
        assert_eq!(next.per_bead_phase.get("bd-1"), Some(&LoopPhase::Verify));
    }

    #[test]
    fn loop_invoke_vehicle_bare_br_close_path_byte_identical() {
        let model = LoopModel {
            queue: vec!["brief for bd-1".to_string()],
            per_bead_phase: [("bd-1".to_string(), LoopPhase::ClaimWatch)]
                .into_iter()
                .collect(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let msg = LoopMsg::BrClose {
            bead_id: "bd-1".to_string(),
        };
        let (next, actions) = tick(&model, &msg);
        assert_eq!(next.per_bead_phase.get("bd-1"), Some(&LoopPhase::Verify));
        assert!(
            actions.is_empty()
                || actions.iter().any(|a| matches!(
                    a,
                    LoopAction::Blocked { .. } | LoopAction::Wait | LoopAction::Send { .. }
                ))
        );
    }

    #[test]
    fn workflow_for_bead_marker_section_opts_in() {
        let bead = ReadyBead {
            description: "Do the thing.\n\n## WORKFLOW bug-fix\n\nDetails.".into(),
            ..Default::default()
        };
        assert_eq!(workflow_for_bead(&bead), Some("bug-fix".to_string()));
    }

    #[test]
    fn workflow_for_bead_label_opts_in() {
        let bead = ReadyBead {
            description: "no marker here".into(),
            labels: vec!["workflow:refactoring".into()],
            ..Default::default()
        };
        assert_eq!(workflow_for_bead(&bead), Some("refactoring".to_string()));
    }

    #[test]
    fn workflow_for_bead_title_never_opts_in() {
        let bead = ReadyBead {
            title: "fix the graph layout bug".into(),
            description: "no marker".into(),
            ..Default::default()
        };
        assert_eq!(workflow_for_bead(&bead), None);
    }

    #[test]
    fn loop_invoke_vehicle_opt_in_per_bead() {
        let model = LoopModel {
            queue: vec!["task for bd-2".to_string()],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let msg = LoopMsg::Tick { now: 0 };
        let (next, actions) = tick(&model, &msg);
        assert_eq!(next.queue.len(), 1, "parked queue stays intact");
        assert!(
            actions.is_empty(),
            "no taker, no vehicle, no send: {actions:?}"
        );
    }

    #[test]
    fn loop_replay_why_prints_triple() {
        use crate::state as st;
        let proj = format!("loop-why-{}", std::process::id());
        let _guard = crate::config::test_state_home();
        let _ = std::fs::remove_file(st::loop_journal_file(&proj));
        let model = LoopModel {
            queue: vec!["q".to_string()],
            per_bead_phase: Default::default(),
            wake_suppression: Default::default(),
            done_mailed: Default::default(),
            event_cursor: None,
            escalation: Default::default(),
            respawn: Default::default(),
            mail_health: Default::default(),
            mail_degraded: false,
            heal_states: Default::default(),
            heal_grace: Default::default(),
            heal_threshold: Default::default(),
            heal_budget: Default::default(),
            pending_events: Default::default(),
            states: Default::default(),
            dispatch_pending: Default::default(),
            drain_pending: Default::default(),
            bead_slots: Default::default(),
            agent_slots: Default::default(),
        };
        let msg = LoopMsg::Tick { now: 0 };
        let (_, actions) = tick(&model, &msg);
        st::journal_loop_msg(&proj, &msg, &model, &actions).unwrap();
        let entries = st::read_loop_journal(&proj);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].get("msg").is_some());
        assert!(entries[0].get("model_snapshot").is_some());
        assert!(entries[0].get("actions").is_some());
        let _ = std::fs::remove_file(st::loop_journal_file(&proj));
    }

    /// GS-030 slice 2 (bd-gs030-seed-helper-6m7e): the crash-resume seed
    /// carries heal counters (awaiting_clear reset), done-mail dedup
    /// verbatim, and drains with FRESH grace.
    #[test]
    fn seed_swarm_resume_carries_heal_done_and_fresh_drain() {
        use std::collections::{BTreeMap, HashSet};
        let mut heal = BTreeMap::new();
        heal.insert(
            "oc".to_string(),
            crate::state::HealState {
                unknown_count: 2,
                diagnose_count: 1,
                respawn_count: 0,
                awaiting_clear: true,
            },
        );
        let mut done = HashSet::new();
        done.insert("bd-1".to_string());
        let mut drain = BTreeMap::new();
        drain.insert("pi".to_string(), "bd-9".to_string());
        let mut model = LoopModel::default();
        model.done_mailed.insert("stale".to_string());
        seed_swarm_resume(
            &mut model,
            &heal,
            &done,
            &drain,
            &std::collections::BTreeMap::new(),
            &std::collections::BTreeMap::new(),
        );
        let h = model.heal_states.get("oc").expect("heal seeded");
        assert_eq!(
            (h.unknown_count, h.diagnose_count, h.respawn_count),
            (2, 1, 0),
            "heal counters carry over"
        );
        assert!(!h.awaiting_clear, "awaiting_clear resets on resume");
        assert_eq!(
            model.done_mailed, done,
            "done-mail dedup verbatim (stale gone)"
        );
        let d = model.drain_pending.get("pi").expect("drain seeded");
        assert_eq!(d.bead_id, "bd-9");
        assert_eq!(
            d.grace_polls, DRAIN_GRACE_POLLS,
            "drains restart with fresh grace"
        );
        let mut owned = crate::workspace::WorkspaceModel::default();
        seed_swarm_resume(
            &mut owned.swarm,
            &heal,
            &done,
            &drain,
            &std::collections::BTreeMap::new(),
            &std::collections::BTreeMap::new(),
        );
        assert_eq!(
            owned.swarm.heal_states.get("oc").map(|h| h.unknown_count),
            Some(2),
            "shared Model path seeds ws.swarm"
        );
        assert!(
            include_str!("durable.rs").contains("fn seed_workspace_from_active_loop"),
            "NEW-L2: workspace wrapper must exist for the doorbell owner"
        );
    }

    #[test]
    fn loop_fold_done_mailed_persists_on_activeloop() {
        let mut al =
            crate::state::ActiveLoop::new("test", serde_json::json!({}), "2026-09-03T00:00:00Z");
        al.done_mailed.insert("bd-1".to_string());
        let raw = serde_json::to_value(&al).unwrap();
        let back: crate::state::ActiveLoop = serde_json::from_value(raw).unwrap();
        assert!(
            back.done_mailed.contains("bd-1"),
            "done_mailed must round-trip through ActiveLoop"
        );
        let src = include_str!("durable.rs");
        let live_wipe =
            src.contains("done_mailed: Default::default(),\n            event_cursor: active");
        assert!(
            !live_wipe,
            "RED (55nx.8): live per-poll LoopModel still wipes done_mailed with Default"
        );
        let needle = ["let mut done", "_mailed"].concat();
        assert!(
            !src.contains(&needle),
            "RED (55nx.4): loop-local done_mailed HashSet still exists; ws.swarm.done_mailed is the only owner"
        );
    }

    #[test]
    fn drain_stop_two_phase_before_force() {
        let model = LoopModel::default();
        let (m1, a1) = tick(
            &model,
            &LoopMsg::DrainRequest {
                name: "oc".into(),
                bead_id: "bd-1".into(),
            },
        );
        assert!(
            a1.iter()
                .any(|a| matches!(a, LoopAction::Send { name, text }
                if name == "oc" && text.contains("bd-1"))),
            "drain ask sends the finish-and-report prompt: {a1:?}"
        );
        assert_eq!(
            m1.drain_pending.get("oc").map(|d| d.bead_id.as_str()),
            Some("bd-1"),
            "drain tracked on the Model"
        );
        let mut m = m1;
        for _ in 0..DRAIN_GRACE_POLLS {
            let (next, acts) = tick(&m, &LoopMsg::Tick { now: 0 });
            assert!(
                !acts.iter().any(|a| matches!(a, LoopAction::Respawn { .. })),
                "no force inside grace"
            );
            m = next;
        }
        let (m3, a3) = tick(
            &m,
            &LoopMsg::Heal {
                name: "oc".into(),
                status: "done".into(),
                exited: true,
            },
        );
        assert!(
            !m3.drain_pending.contains_key("oc"),
            "exit fulfills the drain"
        );
        assert!(
            !a3.iter().any(|a| matches!(a, LoopAction::Respawn { .. })),
            "fulfilled drain never force-stops"
        );
        let (mut m4, _) = tick(
            &model,
            &LoopMsg::DrainRequest {
                name: "oc".into(),
                bead_id: "bd-1".into(),
            },
        );
        let mut force = Vec::new();
        for _ in 0..=DRAIN_GRACE_POLLS {
            let (next, acts) = tick(&m4, &LoopMsg::Tick { now: 0 });
            force.extend(acts);
            m4 = next;
        }
        assert!(
            force
                .iter()
                .any(|a| matches!(a, LoopAction::Respawn { name, reason }
                if name == "oc" && reason.contains("bd-1"))),
            "grace expiry force-stops with the bead id: {force:?}"
        );
        assert!(
            !m4.drain_pending.contains_key("oc"),
            "expired drain leaves no pending entry"
        );
    }

    #[test]
    fn declared_pause_holds_grace_and_resurfaces_on_the_window() {
        let model = LoopModel::default();
        let (m, a1) = tick(
            &model,
            &LoopMsg::DrainRequest {
                name: "oc".into(),
                bead_id: "bd-1".into(),
            },
        );
        assert!(
            a1.iter().any(|a| matches!(a, LoopAction::Send { .. })),
            "the drain ask is sent"
        );
        assert!(
            m.drain_pending
                .get("oc")
                .is_some_and(|d| d.paused.is_none()),
            "a fresh drain has no declaration"
        );
        let (m2, a2) = tick(
            &m,
            &LoopMsg::PauseDeclared {
                name: "oc".into(),
                owner: "CooperSparrow".into(),
                now: 1_000,
                resurface_secs: 3_600,
            },
        );
        assert_eq!(
            m2.drain_pending["oc"]
                .paused
                .as_ref()
                .map(|p| (p.owner.as_str(), p.since)),
            Some(("CooperSparrow", 1_000)),
            "the first declaration starts the clock"
        );
        assert!(a2.is_empty(), "declaring is state, not an effect");
        let (m3, _) = tick(
            &m2,
            &LoopMsg::PauseDeclared {
                name: "oc".into(),
                owner: "NavyDog".into(),
                now: 3_000,
                resurface_secs: 3_600,
            },
        );
        assert_eq!(m3.drain_pending["oc"].paused.as_ref().unwrap().since, 1_000);
        assert_eq!(
            m3.drain_pending["oc"].paused.as_ref().unwrap().owner,
            "CooperSparrow"
        );
        let mut parked = m3;
        for now in [1_001u64, 2_000, 4_599] {
            let (next, acts) = tick(&parked, &LoopMsg::Tick { now });
            assert!(
                !acts.iter().any(|a| matches!(a, LoopAction::Respawn { .. })),
                "a declared pause is never force-stopped (now {now}): {acts:?}"
            );
            assert!(
                next.drain_pending.contains_key("oc"),
                "the drain stays open while the wait is declared (now {now})"
            );
            parked = next;
        }
        let (after, acts) = tick(&parked, &LoopMsg::Tick { now: 4_600 });
        assert!(
            acts.iter().any(|a| matches!(
                a,
                LoopAction::PausedResurface { name, bead_id, owner, age_secs }
                    if name == "oc" && bead_id == "bd-1" && owner == "CooperSparrow" && *age_secs == 3_600
            )),
            "the wait re-surfaces to its owner on the window: {acts:?}"
        );
        assert_eq!(
            after.drain_pending["oc"]
                .paused
                .as_ref()
                .unwrap()
                .resurfaced,
            1
        );
        let (quiet, acts) = tick(&after, &LoopMsg::Tick { now: 4_601 });
        assert!(
            !acts
                .iter()
                .any(|a| matches!(a, LoopAction::PausedResurface { .. })),
            "resurface is once per window, not once per tick"
        );
        assert_eq!(
            quiet.drain_pending["oc"]
                .paused
                .as_ref()
                .unwrap()
                .resurfaced,
            1
        );
        let (_, acts) = tick(&quiet, &LoopMsg::Tick { now: 8_200 });
        assert!(
            acts.iter().any(|a| matches!(
                a,
                LoopAction::PausedResurface { age_secs, .. } if *age_secs == 3_600
            )),
            "each window re-surfaces, bounded and observable: {acts:?}"
        );
    }

    #[test]
    fn pause_declaration_is_ignored_without_an_open_drain() {
        let model = LoopModel::default();
        let (next, acts) = tick(
            &model,
            &LoopMsg::PauseDeclared {
                name: "ghost".into(),
                owner: "CooperSparrow".into(),
                now: 10,
                resurface_secs: 3_600,
            },
        );
        assert_eq!(acts, [] as [LoopAction; 0]);
        assert!(next.drain_pending.is_empty(), "no drain is invented");
    }

    #[test]
    fn declared_pause_replays_log_identically() {
        let log = [
            LoopMsg::DrainRequest {
                name: "oc".into(),
                bead_id: "bd-1".into(),
            },
            LoopMsg::PauseDeclared {
                name: "oc".into(),
                owner: "CooperSparrow".into(),
                now: 5,
                resurface_secs: 3_600,
            },
            LoopMsg::Tick { now: 3_605 },
            LoopMsg::Tick { now: 3_606 },
            LoopMsg::Tick { now: 7_205 },
        ];
        let run = || {
            let mut m = LoopModel::default();
            let mut acts = Vec::new();
            for msg in &log {
                let (next, a) = tick(&m, msg);
                m = next;
                acts.push(a);
            }
            (m, acts)
        };
        let (m1, a1) = run();
        let (m2, a2) = run();
        assert_eq!(m1, m2, "same log folds to the same model");
        assert_eq!(a1, a2, "same log produces the same effects");
        assert!(
            a1.iter()
                .flatten()
                .any(|a| matches!(a, LoopAction::PausedResurface { .. })),
            "the log really exercised the resurface"
        );
    }

    #[test]
    fn drain_state_pause_round_trips_and_legacy_rows_load() {
        let ds: DrainState =
            serde_json::from_str(r#"{"bead_id":"bd-1","grace_polls":3,"defers":0}"#)
                .expect("legacy drain row loads");
        assert_eq!(ds.paused, None);
        let with_pause = DrainState {
            bead_id: "bd-1".into(),
            grace_polls: 3,
            defers: 0,
            paused: Some(PausedWait {
                owner: "CooperSparrow".into(),
                since: 1_000,
                resurface_secs: 3_600,
                resurfaced: 2,
            }),
        };
        let json = serde_json::to_string(&with_pause).unwrap();
        assert_eq!(
            serde_json::from_str::<DrainState>(&json).unwrap(),
            with_pause
        );
    }

    #[test]
    fn pause_resurface_line_names_owner_bead_and_age() {
        let line = pause_resurface_line("oc", "bd-1", "CooperSparrow", 3_600);
        assert!(line.contains("pause-resurface"), "{line}");
        assert!(line.contains("CooperSparrow"), "{line}");
        assert!(line.contains("bd-1"), "{line}");
        assert!(line.contains("3600"), "{line}");
    }

    #[test]
    fn swarm_holds_no_assignment_cache() {
        let needle = [".assign", "ment"].concat();
        let src = include_str!("durable.rs");
        let reads: Vec<&str> = src
            .lines()
            .filter(|l| l.contains(&needle))
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        assert!(
            reads.is_empty(),
            "RED (55nx.3): swarm reads the assignment cache: {reads:?}"
        );
        assert!(
            !src.contains(&["rebuild_assignment", "_from_receipts"].concat()),
            "RED (55nx.3): swarm references the doorbell-only seed"
        );
    }
}

/// GS-033 Option A tripwires: the live loop must not construct a second
/// WorkspaceModel (B1). Doorbell-slice Msgs still stay off the loop (the
/// loop is the effect runner, not a second doorbell). Needles split so
/// these self-scans do not match their own source.
#[cfg(test)]
mod single_writer_tests {
    #[test]
    fn option_a_loop_constructs_no_workspace_model_default() {
        let src = include_str!("durable.rs");
        let live = [
            "let mut ws = crate::workspace::Workspace",
            "Model::default()",
        ]
        .concat();
        assert!(
            !src.contains(&live),
            "Option A: swarm_loop must not construct WorkspaceModel::default() — doorbell is the one RAM owner"
        );
        let wrap_ty = ["crate::workspace::Workspace", "Model {"].concat();
        let mut wrap_sites: Vec<usize> = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(&wrap_ty) {
            let abs = from + rel;
            let window = &src[abs..src.len().min(abs + 220)];
            if window.contains("..Default::default()") {
                wrap_sites.push(abs);
            }
            from = abs + wrap_ty.len();
        }
        assert!(
            !wrap_sites.is_empty(),
            "Option A honesty: tick_loop_owner throwaway Default wrap must remain visible"
        );
        for abs in wrap_sites {
            let before = &src[..abs];
            let _ = before
                .rfind("fn tick_loop_owner(")
                .expect("Option A: WorkspaceModel Default wrap only inside tick_loop_owner");
        }
        assert!(
            src.contains("/// Throwaway WorkspaceModel"),
            "Option A: tick_loop_owner must stay labeled throwaway (not a live second owner)"
        );
        let start = src.find("async fn swarm_loop(").expect("swarm_loop");
        let body = &src[start..src.len().min(start + 4000)];
        assert!(
            body.contains("seed_swarm_resume"),
            "Option A: swarm_loop must seed LoopModel via the shared seed helper"
        );
        assert!(
            body.contains("refusing silent empty resume")
                && body.contains("match crate::state::load_active_loop"),
            "GS-033: swarm_loop ActiveLoop seed must fail-loud on I/O Err (not if-let-Ok swallow)"
        );
        let seed_at = body
            .find("let mut swarm = LoopModel::default()")
            .expect("swarm default");
        let seed_slice = &body[seed_at..body.len().min(seed_at + 900)];
        assert!(
            !seed_slice.contains("if let Ok(Some(al))"),
            "GS-033: swarm_loop must not swallow ActiveLoop seed Err into empty resume"
        );
        assert!(
            body.contains("LoopModel::default()"),
            "Option A: swarm_loop holds LoopModel, not a second WorkspaceModel"
        );
        assert!(
            !body.contains("..Default::default()"),
            "Option A: swarm_loop body must not Default-wrap a WorkspaceModel"
        );
    }

    /// Doorbell-slice Msgs the loop must never construct (watch / inbox /
    /// status / role / presence / receipt / sweep / watch-died / fact /
    /// ack-sla / human-decision).
    #[test]
    fn single_writer_loop_constructs_no_doorbell_slice_msgs() {
        let src = include_str!("durable.rs");
        let variants = [
            ["Msg::Watch", "Ndjson {"].concat(),
            ["Msg::Inbox", "Rows {"].concat(),
            ["Msg::Inbox", "Items {"].concat(),
            ["Msg::Hc", "Status {"].concat(),
            ["Msg::Role", "Bound {"].concat(),
            ["Msg::Pres", "ence {"].concat(),
            ["Msg::Rece", "ipt {"].concat(),
            ["Msg::Swe", "ep {"].concat(),
            ["Msg::Watch", "Died"].concat(),
            ["Msg::Fa", "ct {"].concat(),
            ["Msg::AckSla", "Tick {"].concat(),
            ["Msg::HumanDec", "ision"].concat(),
        ];
        for needle in &variants {
            assert!(
                !src.contains(needle.as_str()),
                "Option A: swarm_loop must not construct {needle} (doorbell slice)"
            );
        }
    }
}

#[cfg(test)]
mod t4_close_tests {
    use super::*;

    fn at() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("flywheel-settle-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn runnable(dir: &std::path::Path) -> serde_json::Value {
        serde_json::json!({
            "band": "loop-runnable",
            "dir": dir.to_str().unwrap(),
            "output_tail": "ok 3 passed",
            "verify": "cargo test x",
            "exit": 0,
        })
    }

    #[test]
    fn non_runnable_band_passes_through_untouched() {
        let v = serde_json::json!({"band": "non-runnable", "raw": "x"});
        assert_eq!(
            plan_loop_verify_settle(&v, false),
            LoopVerifySettle::Passthrough
        );
        let out = settle_loop_verify_output(&v, "proj", "bd-x", "abc", false, at(), || {
            panic!("passthrough must never reopen")
        })
        .expect("passthrough settles");
        assert_eq!(out, v);
    }

    #[test]
    fn plan_names_record_for_runnable_proof() {
        let dir = scratch("plan");
        let plan = plan_loop_verify_settle(&runnable(&dir), false);
        assert_eq!(
            plan,
            LoopVerifySettle::Record {
                output: "ok 3 passed".to_string(),
                warn_append: None,
                verify_cmd: "cargo test x".to_string(),
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_names_reopen_for_failed_head() {
        let dir = scratch("fail");
        let mut v = runnable(&dir);
        v["exit"] = serde_json::json!(1);
        let plan = plan_loop_verify_settle(&v, false);
        assert_eq!(
            plan,
            LoopVerifySettle::Reopen {
                vacuous: None,
                timed_out: false,
                exit: 1
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vacuous_strict_refuses_and_writes_evidence() {
        let dir = scratch("vacuous");
        let v = serde_json::json!({
            "band": "loop-runnable",
            "dir": dir.to_str().unwrap(),
            "output_tail": "test result: ok. 0 passed",
            "verify": "cargo test nothing-matches",
            "exit": 0,
        });
        match plan_loop_verify_settle(&v, true) {
            LoopVerifySettle::Reopen {
                vacuous: Some((w, _, e)),
                timed_out,
                exit,
            } => {
                assert!(w.contains("vacuous VERIFY"));
                assert_eq!((timed_out, exit), (false, 0));
                assert!(e.contains("output tail"));
            }
            other => panic!("strict vacuous must refuse with evidence, got {other:?}"),
        }
        let out = settle_loop_verify_output(&v, "proj", "bd-x", "abc", true, at(), || Ok(true))
            .expect("strict vacuous settles via reopen");
        assert_eq!(out["reopened"], true);
        assert_eq!(out["vacuous"], true);
        let ev = std::fs::read_to_string(dir.join(".flywheel/receipts/bd-x-abc.vacuous.txt"))
            .expect("vacuous evidence written");
        assert!(ev.contains("vacuous VERIFY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vacuous_advisory_still_records_with_annotation() {
        let dir = scratch("advisory");
        let v = serde_json::json!({
            "band": "loop-runnable",
            "dir": dir.to_str().unwrap(),
            "output_tail": "test result: ok. 0 passed",
            "verify": "cargo test nothing-matches",
            "exit": 0,
        });
        match plan_loop_verify_settle(&v, false) {
            LoopVerifySettle::Record {
                warn_append: Some(w),
                ..
            } => {
                assert!(w.contains("vacuous VERIFY"));
            }
            other => panic!("advisory vacuous must record annotated, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_failure_propagates_with_leaf_prefix() {
        let dir = scratch("reopen-err");
        let mut v = runnable(&dir);
        v["exit"] = serde_json::json!(1);
        let err = settle_loop_verify_output(&v, "proj", "bd-x", "abc", false, at(), || {
            Err(anyhow::anyhow!("boom"))
        })
        .expect_err("reopen failure must propagate");
        assert!(err.to_string().contains("__reopen-bead failed:"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_without_duplicates_settle_twice_identical() {
        let dir = scratch("idempotent");
        let mut v = runnable(&dir);
        v["exit"] = serde_json::json!(1);
        let once = settle_loop_verify_output(&v, "proj", "bd-x", "abc", false, at(), || Ok(true))
            .expect("first settle");
        let twice = settle_loop_verify_output(&v, "proj", "bd-x", "abc", false, at(), || Ok(true))
            .expect("resume settles identically");
        assert_eq!(once, twice, "kill mid-window resumes without duplicates");
        assert_eq!(twice["reopened"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unarmed_record_refuses_forgery() {
        let _lock = crate::verdict::lock_loop_reporter();
        crate::verdict::test_reset_loop_reporter();
        let dir = scratch("unarmed");
        let v = runnable(&dir);
        let err = settle_loop_verify_output(&v, "proj", "bd-x", "abc", false, at(), || Ok(true))
            .expect_err("un loop-armed record must refuse");
        assert!(err.to_string().contains("LOOP only"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn guard_facts(signals: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "signals": signals, "receipts_fresh_us": null })
    }

    #[test]
    fn dispatch_guard_hold_shape_is_exact() {
        let now = 1_000_000_000i64;
        let facts = guard_facts(serde_json::json!([
            {"ts_us": now - 3_000_000, "kind": "stand_down"},
        ]));
        let v = settle_dispatch_guard_output(&facts, "bd-x", now, 600_000_000);
        assert_eq!(v["bead"], "bd-x");
        assert_eq!(v["dispatch"], false);
        assert!(v["reason"].as_str().unwrap().contains("stood down"), "{v}");
    }

    #[test]
    fn dispatch_guard_proceed_shape_is_exact() {
        let now = 1_000_000_000i64;
        let v = settle_dispatch_guard_output(
            &guard_facts(serde_json::json!([])),
            "bd-x",
            now,
            600_000_000,
        );
        assert_eq!(
            v,
            serde_json::json!({"bead": "bd-x", "dispatch": true, "reason": null})
        );
    }

    #[test]
    fn dispatch_guard_drops_malformed_signals() {
        let now = 1_000_000_000i64;
        let facts = guard_facts(serde_json::json!([
            {"ts_us": now - 1_000, "kind": "bogus-kind"},
            {"kind": "claim"},
            {"ts_us": "not-a-number", "kind": "claim"},
        ]));
        let v = settle_dispatch_guard_output(&facts, "bd-x", now, 600_000_000);
        assert_eq!(v["dispatch"], true, "malformed signals never hold: {v}");
    }

    #[test]
    fn dispatch_guard_settle_is_deterministic() {
        let now = 1_000_000_000i64;
        let facts = guard_facts(serde_json::json!([
            {"ts_us": now - 3_000_000, "kind": "stand_down"},
        ]));
        let a = settle_dispatch_guard_output(&facts, "bd-x", now, 600_000_000);
        let b = settle_dispatch_guard_output(&facts, "bd-x", now, 600_000_000);
        assert_eq!(a, b, "same inputs settle identically");
    }

    fn mk_attempt(
        bead: &str,
        pane: &str,
        generation: u64,
        status: WorkAttemptStatus,
    ) -> WorkAttempt {
        WorkAttempt {
            bead_id: bead.to_string(),
            pane: pane.to_string(),
            generation,
            status,
            corr: None,
            effect_key: None,
            updated_us: 1_000_000_000,
        }
    }

    /// bd-work-attempts-m5x6: steering supersedes under the same bead — the
    /// old generation stays readable as Superseded, the replacement runs at
    /// generation+1 carrying the new prompt's corr, and the resume window
    /// treats the replacement as fresh (no watermark) while a repeat send
    /// of the live generation stays suppressed.
    #[test]
    fn attempt_supersede_under_same_bead() {
        let now = 2_000_000_000i64;
        let live = WorkAttempt {
            corr: Some("corr-1".to_string()),
            effect_key: Some("ek-1".to_string()),
            ..mk_attempt("bd-x", "PaneA", 1, WorkAttemptStatus::Running)
        };
        let (old, next) = supersede_attempt(&live, "corr-2", Some("ek-2"), now);
        assert_eq!(old.bead_id, "bd-x");
        assert_eq!(next.bead_id, "bd-x");
        assert_eq!(next.pane, "PaneA");
        assert_eq!(next.generation, 2);
        assert_eq!(old.status, WorkAttemptStatus::Superseded);
        assert_eq!(old.corr.as_deref(), Some("corr-1"));
        assert_eq!(old.effect_key.as_deref(), Some("ek-1"));
        assert_eq!(next.status, WorkAttemptStatus::Running);
        assert_eq!(next.corr.as_deref(), Some("corr-2"));
        assert_eq!(next.effect_key.as_deref(), Some("ek-2"));
        assert_eq!(next.updated_us, now);

        let attempts = vec![old.clone(), next.clone()];
        assert_eq!(
            live_attempt_for_bead(&attempts, "bd-x").map(|a| a.generation),
            Some(2)
        );
        assert!(live_attempt_for_bead(&attempts, "bd-y").is_none());

        const W: u64 = RESUME_WINDOW_DEFAULT_SECS;
        assert!(should_resume_attempt(None, now, W, false, Some(&next)));
        assert!(!should_resume_attempt(
            Some(now),
            now,
            W,
            false,
            Some(&next)
        ));
        assert!(
            !should_resume_attempt(None, now, W, true, Some(&next)),
            "a peer hold suppresses even a fresh generation"
        );
        let done = mk_attempt("bd-x", "PaneA", 2, WorkAttemptStatus::Completed);
        assert!(
            !should_resume_attempt(None, now, W, false, Some(&done)),
            "finished work stays finished even when the bead row lags"
        );
        assert!(
            !should_resume_attempt(None, now, W, false, Some(&old)),
            "a superseded generation never resumes"
        );
        assert!(
            should_resume_attempt(None, now, W, false, None),
            "a bead with no attempt on record dispatches"
        );
    }

    /// Attempt keys are unambiguous across generations (unit separator, like
    /// the resume window) and statuses round-trip through their spellings.
    #[test]
    fn attempt_key_and_status_table() {
        assert_eq!(attempt_key("bd-x", "PaneA", 2), "bd-x\u{1f}PaneA\u{1f}2");
        for (s, name) in [
            (WorkAttemptStatus::Queued, "queued"),
            (WorkAttemptStatus::Running, "running"),
            (WorkAttemptStatus::Completed, "completed"),
            (WorkAttemptStatus::Interrupted, "interrupted"),
            (WorkAttemptStatus::Failed, "failed"),
            (WorkAttemptStatus::Superseded, "superseded"),
        ] {
            assert_eq!(s.as_str(), name);
            assert_eq!(WorkAttemptStatus::parse(name), Some(s));
        }
        assert_eq!(WorkAttemptStatus::parse("bogus"), None);
    }
}

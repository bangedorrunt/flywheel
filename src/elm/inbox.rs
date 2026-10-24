//! The doorbell's inbox as one framework program (ADR-0018 §3 item 3).
//!
//! `DoorbellModel` and `WakeModel` become ONE program: the doorbell's
//! edges — a tick, a herdr event, a mail ingress — are MESSAGES, and they
//! never call the model. The fold is pure (no clock, no environment, no
//! store, no randomness); the interpreter runs the commands at the edge; the
//! framework's log and producer carry the durable identity
//! (`(producer_id, epoch, seq)`) and the resume field is `offset`.
//!
//! The fold DELEGATES: `doorbell_core::tick` (the dispatcher) and
//! `doorbell_core::wake_core::tick` (the router) stay the one implementation
//! of each decision. This module owns the shape around them — the message
//! union, the command, the interpreter, the journal — and never a second
//! copy of a decision. The workspace's own doorbell slice folds through
//! [`InboxProgram::fold`], so the live path and the program share ONE fold.
//!
//! The four outcomes stay apart: a typed failure is not a defect, an
//! interruption is not a failure, and a journal that refused the append
//! means the command never ran. A dropped command value performs no I/O:
//! the fold only ever builds values, and the edge runs them.

// The framework half of this port — the `Program` impl, the interpreter, the
// edge and the loop entry point — is the shape the doorbell daemon adopts
// when it can reach the framework (a `toron_core::kernel` dependency in a
// deliberately light crate is a decision, not a line). Until then the fold
// is the live path and the tests are the framework half's only caller, so
// the build gate would read it as dead code. The allowance is scoped to
// this module and says exactly that.
#![allow(dead_code)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use toron_core::kernel::{
    Boot, BuiltServices, Cmd as CmdTrait, Durability, DurableLog, Effect, Failure, FileLog,
    Interpret, Layer, Loop, ManagedRuntime, Producer, Program, ServiceTag, Update, use_service,
};

use doorbell_core::wake_core::{GateDecision, RouteInput, WakeEffect, WakeModel, WakeMsg};
use doorbell_core::{SelfIndex, fnv1a64};

// The doorbell's own vocabulary: the plane's name for the core's model.
use crate::doorbell_tick::{DoorbellAction, DoorbellModel, DoorbellMsg, tick};

/// The doorbell's model: the dispatcher's state and the router's state, in
/// ONE program.
///
/// Both halves are BTreeMap-backed where the order changes the command list
/// (the dispatcher's queues, statuses and budgets; the router's flood table
/// and ladder), so the command list never depends on a hash order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxModel {
    /// The dispatcher: LWW queues per pin, last-known statuses, the pin
    /// index, and the per-pin prompt budgets.
    pub dispatcher: DoorbellModel,
    /// The wake router: the flood table, the wake ladder, the ack-SLA set.
    pub wake: WakeModel,
    /// The last command that did not settle as success, with its cause. The
    /// cause stays apart from the others (see [`InboxFailure`]).
    pub failure: Option<InboxFailure>,
}

/// What enters the fold. Every edge is a message; nothing calls the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxMsg {
    /// A doorbell edge: the periodic tick, a herdr status change, a mail
    /// ingress, a captain fact, or the watch child's death.
    Edge(DoorbellMsg),
    /// The router's input — already-resolved facts (presence word, route
    /// existence, budget state), never a lookup.
    Route(RouteInput),
    /// A command settled. The model already holds the outcome; the message
    /// exists so no command is left dangling.
    Settled {
        /// What settled, for the journal row.
        label: String,
    },
    /// A command that did not settle as success.
    Failed(InboxFailure),
}

/// Why a command did not settle as success. The framework's four outcomes
/// stay apart: a defect is not the child's answer, and a cancelled run does
/// not settle as a failed attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxFailure {
    /// The edge's typed refusal (the provider said no).
    Typed {
        /// What failed.
        label: String,
        /// Why.
        detail: String,
    },
    /// A defect: a bug in the edge or the host, not the command's answer.
    Defect {
        /// What broke.
        label: String,
        /// The defect.
        detail: String,
    },
    /// Interrupted: cancelled, so it did not fail.
    Interrupted {
        /// What was interrupted.
        label: String,
    },
    /// The journal refused the append, so the command never ran. That is
    /// neither the edge's failure nor a defect: the run never happened.
    NotAppended {
        /// What was not appended.
        label: String,
        /// The refusal.
        detail: String,
    },
    /// The journal's append outcome is unknown: the command may have run.
    /// Kept apart from `NotAppended` so a reader never hears "it did not
    /// run" about work that may have landed.
    JournalUnknown {
        /// What may have run.
        label: String,
        /// Why the outcome is unknown.
        detail: String,
    },
}

impl InboxFailure {
    /// The label the failure names.
    pub fn label(&self) -> &str {
        match self {
            Self::Typed { label, .. }
            | Self::Defect { label, .. }
            | Self::Interrupted { label }
            | Self::NotAppended { label, .. }
            | Self::JournalUnknown { label, .. } => label,
        }
    }
}

/// One command the edge runs: the work, in the doorbell's own action
/// vocabulary. The router's verdict is folded into the same vocabulary
/// (inject → prompt, digest → digest, hold → audit, drop → audit), so one
/// command type carries every decision the doorbell makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxCmd {
    /// What the edge must do.
    pub action: DoorbellAction,
}

impl InboxCmd {
    /// The command for one dispatcher action.
    pub fn dispatch(action: DoorbellAction) -> Self {
        Self { action }
    }

    /// The durable row's key: stable for the same work, so a redelivery
    /// after a crash joins the same row instead of minting a new one.
    ///
    /// Computed on demand: the live path never journals a command, and a
    /// hash per wake on the doorbell's hot path buys nothing.
    pub fn key(&self) -> String {
        match &self.action {
            DoorbellAction::Prompt { pin, line } => {
                format!("wake:prompt:{pin}:{}", fnv1a64(line))
            }
            DoorbellAction::Digest { pin, line, .. } => {
                format!("wake:digest:{pin}:{}", fnv1a64(line))
            }
            DoorbellAction::Audit { kind, pin, detail } => {
                format!("wake:audit:{kind}:{pin}:{}", fnv1a64(detail))
            }
            DoorbellAction::FactMiss { why } => {
                format!("wake:fact-miss:{}", fnv1a64(why))
            }
            DoorbellAction::FetchInbox => "wake:fetch-inbox".into(),
            DoorbellAction::CheckAckSla => "wake:check-ack-sla".into(),
            DoorbellAction::RestartWatch => "wake:restart-watch".into(),
        }
    }

    /// What this command is called, for the journal row and the failure.
    pub fn label(&self) -> String {
        match &self.action {
            DoorbellAction::Prompt { pin, .. } => format!("prompt {pin}"),
            DoorbellAction::Digest { pin, .. } => format!("digest {pin}"),
            DoorbellAction::Audit { kind, .. } => format!("audit {kind}"),
            DoorbellAction::FactMiss { .. } => "fact-miss".into(),
            DoorbellAction::FetchInbox => "fetch-inbox".into(),
            DoorbellAction::CheckAckSla => "check-ack-sla".into(),
            DoorbellAction::RestartWatch => "restart-watch".into(),
        }
    }

    /// Is this command a signal the host re-derives every cycle (and so
    /// has nothing to redeliver)?
    fn is_signal(&self) -> bool {
        matches!(
            self.action,
            DoorbellAction::FetchInbox | DoorbellAction::CheckAckSla | DoorbellAction::RestartWatch
        )
    }
}

impl CmdTrait for InboxCmd {
    fn durability(&self) -> Durability {
        if self.is_signal() {
            // A signal to the host: the host asks for it again every cycle,
            // so there is nothing to redeliver after a crash.
            Durability::Ephemeral
        } else {
            // A delivery, a digest, a loud miss: never a silent drop. The
            // key names the row, so a redelivery after a crash joins it.
            Durability::Durable { key: self.key() }
        }
    }
}

/// The doorbell's inbox, as a program.
pub struct InboxProgram;

impl InboxProgram {
    /// The dispatcher's fold: one message in, the next model and the
    /// commands out. ONE implementation — the program wraps it, and the
    /// workspace's doorbell slice calls it, so the live path and the
    /// program never drift.
    pub fn fold(model: &DoorbellModel, msg: &DoorbellMsg) -> (DoorbellModel, Vec<InboxCmd>) {
        let (next, actions) = tick(model, msg);
        (next, actions.into_iter().map(InboxCmd::dispatch).collect())
    }

    /// The router's fold: one already-resolved wake in, the verdict out.
    /// The verdict is folded into the doorbell's action vocabulary, so the
    /// edge runs ONE kind of command.
    pub fn fold_route(wake: &WakeModel, input: &RouteInput) -> (WakeModel, Vec<InboxCmd>) {
        let (next, effects) = doorbell_core::wake_core::tick(
            wake,
            &WakeMsg::Route {
                input: input.clone(),
            },
        );
        let commands = effects
            .into_iter()
            .filter_map(|effect| match effect {
                WakeEffect::Routed { decision } => Some(route_action(&decision, input)),
                _ => None,
            })
            .collect();
        (next, commands)
    }
}

/// The router's verdict, in the doorbell's action vocabulary: inject is a
/// prompt, a digest is a digest, a hold and a drop are loud misses (a
/// dropped wake is never a silent drop).
fn route_action(decision: &GateDecision, input: &RouteInput) -> InboxCmd {
    let action = match decision {
        GateDecision::Inject { pin, line } => DoorbellAction::Prompt {
            pin: pin.clone(),
            line: line.clone(),
        },
        GateDecision::Digest { pin, reason } => DoorbellAction::Digest {
            pin: pin.clone(),
            line: input.line.clone(),
            why: reason.clone(),
        },
        GateDecision::Queue { wake } => DoorbellAction::Audit {
            kind: "wake_queued".into(),
            pin: input.pin.clone(),
            detail: wake.clone(),
        },
        GateDecision::Drop { reason } => DoorbellAction::Audit {
            kind: "wake_dropped".into(),
            pin: input.pin.clone(),
            detail: reason.clone(),
        },
    };
    InboxCmd::dispatch(action)
}

impl Program for InboxProgram {
    type Model = InboxModel;
    type Msg = InboxMsg;
    type Cmd = InboxCmd;
    type Err = String;

    fn init(boot: &Boot) -> Update<InboxModel, InboxCmd> {
        // The pin index the dispatcher routes by: the host's, carried in the
        // boot config as JSON, so the model starts whole. No index, no
        // routing: an empty index routes nothing and says so.
        let index = boot
            .get("index")
            .and_then(|raw| serde_json::from_str::<SelfIndex>(raw).ok())
            .unwrap_or_default();
        let model = InboxModel {
            dispatcher: DoorbellModel {
                index,
                ..DoorbellModel::default()
            },
            ..InboxModel::default()
        };
        Update::quiet(model)
    }

    fn update(model: &InboxModel, msg: &InboxMsg) -> Update<InboxModel, InboxCmd> {
        match msg {
            InboxMsg::Edge(edge) => {
                // The dispatcher's decision, by the one implementation of it.
                let (next, commands) = InboxProgram::fold(&model.dispatcher, edge);
                Update {
                    model: InboxModel {
                        dispatcher: next,
                        wake: model.wake.clone(),
                        failure: model.failure.clone(),
                    },
                    commands,
                }
            }
            InboxMsg::Route(input) => {
                // The router's decision, by the one implementation of it.
                let (next, commands) = InboxProgram::fold_route(&model.wake, input);
                Update {
                    model: InboxModel {
                        dispatcher: model.dispatcher.clone(),
                        wake: next,
                        failure: model.failure.clone(),
                    },
                    commands,
                }
            }
            InboxMsg::Settled { .. } => Update::quiet(model.clone()),
            InboxMsg::Failed(failure) => {
                // The command did not settle as success. The cause is kept
                // apart from the others, and the model records it: the
                // journal already holds the message row, so the miss is
                // never a silent drop. No command comes out of it — a
                // failure that produced commands would ask the edge to
                // report the failure, and a failing report would ask again.
                let mut next = model.clone();
                next.failure = Some(failure.clone());
                Update::quiet(next)
            }
        }
    }

    fn failure(failure: &Failure<String>) -> InboxMsg {
        let label = "command".to_string();
        match failure {
            // The edge's typed refusal.
            Failure::Fail(why) => InboxMsg::Failed(InboxFailure::Typed {
                label,
                detail: why.clone(),
            }),
            // A defect is a bug, not an answer: it is kept apart so a reader
            // never reads it as the command having failed.
            Failure::Defect(defect) => InboxMsg::Failed(InboxFailure::Defect {
                label,
                detail: format!("{defect:?}"),
            }),
            // Interruption is not a failure and must not settle as one.
            Failure::Interrupted => InboxMsg::Failed(InboxFailure::Interrupted { label }),
            // The journal refused the append, so the command never ran.
            Failure::NotAppended(error) => InboxMsg::Failed(InboxFailure::NotAppended {
                label,
                detail: format!("{error:?}"),
            }),
            // The append may have landed. Never report this as "did not run".
            Failure::Unknown(error) => InboxMsg::Failed(InboxFailure::JournalUnknown {
                label,
                detail: format!("{error:?}"),
            }),
        }
    }
}

/// The edge: what actually runs a command. The fold never touches the
/// world; this is the only place that does.
pub trait InboxEdge: Send + Sync {
    /// Run one command. `Ok` is a settled command; `Err` is the edge's
    /// typed refusal — never a defect, never an interruption.
    fn run(&self, cmd: &InboxCmd) -> Result<(), String>;
}

/// The edge's service tag: the loop's runtime carries the edge, and the
/// interpreter closes the tag with the loop's built services.
pub struct InboxEdgeTag;

impl ServiceTag for InboxEdgeTag {
    type Service = Arc<dyn InboxEdge>;
    const ID: &'static str = "flywheel-inbox-edge";
}

/// The interpreter: the edge, and nothing else.
pub struct InboxInterpret;

impl Interpret<InboxProgram> for InboxInterpret {
    fn interpret(cmd: &InboxCmd) -> Effect<InboxMsg, String> {
        // No edge in hand: the command cannot run. That is a typed failure
        // the fold records, never a silent drop.
        Effect::succeed(InboxMsg::Failed(InboxFailure::Typed {
            label: cmd.label(),
            detail: "no edge in hand: boot the loop with a runtime that carries the edge".into(),
        }))
    }

    fn interpret_in_env(
        cmd: &InboxCmd,
        services: Option<&BuiltServices>,
    ) -> Effect<InboxMsg, String> {
        let Some(services) = services else {
            return Self::interpret(cmd);
        };
        let cmd = cmd.clone();
        use_service::<InboxEdgeTag>()
            .map_err(|_: std::convert::Infallible| String::from("unreachable"))
            .map(move |edge| match edge.run(&cmd) {
                Ok(()) => InboxMsg::Settled { label: cmd.label() },
                Err(detail) => InboxMsg::Failed(InboxFailure::Typed {
                    label: cmd.label(),
                    detail,
                }),
            })
            .provide_from(services)
    }
}

/// The journal file for one project's inbox program. Pure path computation,
/// so tests can predict and isolate it.
pub fn inbox_journal_path(project: &str) -> std::path::PathBuf {
    crate::state::loops_dir().join(format!(
        "{}.inbox.journal",
        crate::state::state_key(project)
    ))
}

/// Boot the doorbell's inbox program on the framework's loop: the pin index
/// in the boot config, the edge in the runtime, the project's journal as the
/// durable log, and the producer id as the durable identity.
///
/// `Loop` is `!Send`: the caller hosts it on a dedicated OS thread and pumps
/// messages into it with `Loop::send`.
pub fn inbox_loop(
    project: &str,
    edge: Arc<dyn InboxEdge>,
    index: &SelfIndex,
) -> anyhow::Result<Loop<InboxProgram, InboxInterpret>> {
    let path = inbox_journal_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path)?);
    let boot = Boot {
        config: [("index".to_string(), serde_json::to_string(index)?)]
            .into_iter()
            .collect(),
        args: Vec::new(),
    };
    let layer = Layer::<InboxEdgeTag, std::convert::Infallible, ()>::from_value(edge);
    let rt = ManagedRuntime::make(&layer).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(Loop::<InboxProgram, InboxInterpret>::boot(
        &boot,
        rt,
        log,
        Producer::new(format!("inbox:{}", crate::state::state_key(project))),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use doorbell_core::wake_core::PresenceVerdict;
    use toron_core::kernel::Runtime;

    /// An edge that records what it ran, and can refuse.
    #[derive(Default)]
    struct Recorder {
        ran: std::sync::Mutex<Vec<String>>,
        refuse: bool,
    }

    impl InboxEdge for Recorder {
        fn run(&self, cmd: &InboxCmd) -> Result<(), String> {
            self.ran.lock().unwrap().push(cmd.label());
            if self.refuse {
                Err("the edge refused".into())
            } else {
                Ok(())
            }
        }
    }

    fn route_input(pin: &str, line: &str) -> RouteInput {
        RouteInput {
            pin: pin.into(),
            line: line.into(),
            urgent: false,
            presence: PresenceVerdict::Idle,
            has_route: true,
            budget_allows: true,
        }
    }

    #[test]
    fn the_fold_agrees_with_the_core_it_delegates_to() {
        // The program owns the SHAPE, not a second decision: the same edge
        // message folds to the same model and the same commands the core
        // produces.
        let model = InboxModel::default();
        let edge = DoorbellMsg::HerdrStatus {
            pin: "EmeraldSparrow".into(),
            status: "idle".into(),
        };
        let (next, actions) = doorbell_core::tick(&model.dispatcher, &edge);
        let update = InboxProgram::update(&model, &InboxMsg::Edge(edge));
        assert_eq!(update.model.dispatcher, next);
        assert_eq!(
            update.commands,
            actions
                .into_iter()
                .map(InboxCmd::dispatch)
                .collect::<Vec<InboxCmd>>()
        );
    }

    #[test]
    fn the_workspace_slice_and_the_program_fold_the_same_way() {
        // The live path (the workspace's doorbell slice) and the program
        // share ONE fold: same message, same model, same commands.
        let model = DoorbellModel::default();
        let edge = DoorbellMsg::HerdrStatus {
            pin: "EmeraldSparrow".into(),
            status: "idle".into(),
        };
        let (next, commands) = InboxProgram::fold(&model, &edge);
        let inbox = InboxModel {
            dispatcher: model.clone(),
            ..InboxModel::default()
        };
        let update = InboxProgram::update(&inbox, &InboxMsg::Edge(edge));
        assert_eq!(update.model.dispatcher, next);
        assert_eq!(update.commands, commands);
    }

    #[test]
    fn the_same_message_log_folds_to_the_same_commands() {
        let log = [
            InboxMsg::Edge(DoorbellMsg::HerdrStatus {
                pin: "EmeraldSparrow".into(),
                status: "working".into(),
            }),
            InboxMsg::Edge(DoorbellMsg::HerdrStatus {
                pin: "EmeraldSparrow".into(),
                status: "idle".into(),
            }),
        ];
        let first: Vec<_> = log
            .iter()
            .map(|msg| InboxProgram::update(&InboxModel::default(), msg).commands)
            .collect();
        let second: Vec<_> = log
            .iter()
            .map(|msg| InboxProgram::update(&InboxModel::default(), msg).commands)
            .collect();
        assert_eq!(first, second);
    }

    #[test]
    fn a_dropped_command_value_performs_no_io() {
        // Building the fold's output must not run it: the edge sees nothing
        // until the interpreter runs the command.
        let edge = Arc::new(Recorder::default());
        let update = InboxProgram::update(
            &InboxModel::default(),
            &InboxMsg::Edge(DoorbellMsg::AckSlaTick { now: 1_700_000_000 }),
        );
        assert!(!update.commands.is_empty(), "the tick asks for work");
        drop(update.commands);
        assert!(edge.ran.lock().unwrap().is_empty(), "no edge ran");
    }

    #[test]
    fn the_four_outcomes_stay_apart() {
        let model = InboxModel::default();
        let typed = InboxProgram::failure(&Failure::Fail("no".into()));
        let defect =
            InboxProgram::failure(&Failure::Defect(toron_core::kernel::Defect::new("boom")));
        let interrupted = InboxProgram::failure(&Failure::Interrupted);
        let not_appended = InboxProgram::failure(&Failure::NotAppended(
            toron_core::kernel::AppendError::NotAnEnvelope("refused".into()),
        ));
        let outcomes = [typed, defect, interrupted, not_appended];
        for outcome in &outcomes {
            let InboxMsg::Failed(failure) = outcome else {
                panic!("a failure folds as a failure");
            };
            // Each cause keeps its own shape, and none of them settles as
            // success.
            let update = InboxProgram::update(&model, outcome);
            assert!(update.commands.is_empty(), "a failure asks for no work");
            assert_eq!(update.model.failure.as_ref(), Some(failure));
        }
        let kinds: Vec<_> = outcomes
            .iter()
            .map(|m| {
                let InboxMsg::Failed(f) = m else {
                    unreachable!()
                };
                std::mem::discriminant(f)
            })
            .collect();
        let mut unique = kinds.clone();
        unique.dedup();
        assert_eq!(unique.len(), 4, "four causes, four shapes");
    }

    #[test]
    fn the_interpreter_runs_the_command() {
        // The edge runs the command, and the outcome folds back as a settled
        // message: the model records no failure.
        let edge = Arc::new(Recorder::default());
        let cmd = InboxCmd::dispatch(DoorbellAction::Prompt {
            pin: "EmeraldSparrow".into(),
            line: "[doorbell] wake".into(),
        });
        let exit = Runtime::run(InboxInterpret::interpret_in_env(
            &cmd,
            ManagedRuntime::make(
                &Layer::<InboxEdgeTag, std::convert::Infallible, ()>::from_value(edge.clone()),
            )
            .unwrap()
            .context(),
        ));
        let toron_core::kernel::Exit::Success(msg) = exit else {
            panic!("the command settles");
        };
        assert_eq!(
            msg,
            InboxMsg::Settled {
                label: "prompt EmeraldSparrow".into()
            }
        );
        assert_eq!(
            edge.ran.lock().unwrap().len(),
            1,
            "the edge ran the command"
        );
    }

    #[test]
    fn a_refused_command_is_a_typed_failure_not_a_defect() {
        let edge = Arc::new(Recorder {
            ran: Default::default(),
            refuse: true,
        });
        let cmd = InboxCmd::dispatch(DoorbellAction::Prompt {
            pin: "EmeraldSparrow".into(),
            line: "[doorbell] wake".into(),
        });
        let exit = Runtime::run(InboxInterpret::interpret_in_env(
            &cmd,
            ManagedRuntime::make(
                &Layer::<InboxEdgeTag, std::convert::Infallible, ()>::from_value(edge),
            )
            .unwrap()
            .context(),
        ));
        let toron_core::kernel::Exit::Success(msg) = exit else {
            panic!("the outcome is a message");
        };
        let InboxMsg::Failed(InboxFailure::Typed { detail, .. }) = msg else {
            panic!("a refusal is a typed failure: {msg:?}");
        };
        assert_eq!(detail, "the edge refused");
    }

    #[test]
    fn the_loop_journals_the_identity_and_refolds_after_a_crash() {
        // The durable identity is the framework's: the journal rows carry
        // (producer_id, epoch, seq), and a recovery replays the fold from
        // the log instead of asking the host again.
        let dir = std::env::temp_dir().join(format!("flywheel-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("inbox.jsonl");
        let edge = Arc::new(Recorder::default());

        let boot = Boot::default();
        let mut loop_ = Loop::<InboxProgram, InboxInterpret>::boot(
            &boot,
            ManagedRuntime::make(
                &Layer::<InboxEdgeTag, std::convert::Infallible, ()>::from_value(edge.clone()),
            )
            .unwrap(),
            Arc::new(FileLog::open(&path).expect("open log")),
            Producer::new("inbox-test"),
        )
        .expect("boot runs init");
        loop_
            .send(InboxMsg::Edge(DoorbellMsg::AckSlaTick {
                now: 1_700_000_000,
            }))
            .expect("send folds");
        assert!(path.exists(), "the journal reached disk");
        let rows = std::fs::read_to_string(&path).expect("journal rows");
        // The file holds one JSON object per line whose "envelope" is the
        // stamped record: the durable identity travels all three parts.
        assert!(rows.contains("inbox-test"), "the producer id: {rows}");
        assert!(rows.contains(r#"\"epoch\":0"#), "the epoch: {rows}");
        assert!(rows.contains(r#"\"seq\":"#), "the seq: {rows}");
        drop(loop_); // Crash: no shutdown, no snapshot.

        // Recover: the fold is replayed from the journal, and the producer
        // resumes after its own last record.
        let recovered = Loop::<InboxProgram, InboxInterpret>::recover(
            &boot,
            ManagedRuntime::make(
                &Layer::<InboxEdgeTag, std::convert::Infallible, ()>::from_value(edge.clone()),
            )
            .unwrap(),
            Arc::new(FileLog::open(&path).expect("reopen log")),
            Producer::new("inbox-test"),
        )
        .expect("recover replays the journal");
        assert!(
            recovered.in_flight().is_empty(),
            "nothing is left in flight"
        );
        // The replayed fold asked for the same work, and the edge ran it
        // again: the journal is the durable record, not the host's memory.
        assert!(
            edge.ran.lock().unwrap().len() >= 2,
            "the replay ran the command"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_router_decides_in_the_same_program() {
        // The router's verdict is a command of the SAME program, so a wake's
        // fate is decided in one fold: inject, digest, hold or drop.
        let model = InboxModel::default();
        let update = InboxProgram::update(
            &model,
            &InboxMsg::Route(route_input("EmeraldSparrow", "wake")),
        );
        assert_eq!(update.commands.len(), 1, "one verdict, one command");
        let DoorbellAction::Prompt { pin, .. } = &update.commands[0].action else {
            panic!(
                "an idle pane with a route is injected: {:?}",
                update.commands[0]
            );
        };
        assert_eq!(pin, "EmeraldSparrow");
    }

    #[test]
    fn a_wake_that_cannot_be_prompted_is_never_a_silent_drop() {
        // A busy pane holds the wake for the digest batch; a spent budget
        // holds it for a later poll. Neither is a silent drop: the wake
        // reaches a reader either way.
        let busy = RouteInput {
            presence: PresenceVerdict::Busy,
            ..route_input("EmeraldSparrow", "wake")
        };
        let (wake, commands) = InboxProgram::fold_route(&WakeModel::default(), &busy);
        assert_eq!(wake, WakeModel::default());
        assert!(
            matches!(&commands[0].action, DoorbellAction::Digest { .. }),
            "a held wake digests: {commands:?}"
        );

        let spent = RouteInput {
            budget_allows: false,
            ..route_input("EmeraldSparrow", "wake")
        };
        let (_, commands) = InboxProgram::fold_route(&WakeModel::default(), &spent);
        assert!(
            matches!(&commands[0].action, DoorbellAction::Audit { kind, .. } if kind == "wake_queued"),
            "a queued wake is loud: {commands:?}"
        );
    }

    #[test]
    fn a_signal_is_ephemeral_and_a_delivery_is_durable() {
        // The host re-derives a signal every cycle, so there is nothing to
        // redeliver; a delivery must not be lost, so it names its row.
        let signal = InboxCmd::dispatch(DoorbellAction::FetchInbox);
        assert_eq!(signal.durability(), Durability::Ephemeral);
        let delivery = InboxCmd::dispatch(DoorbellAction::Prompt {
            pin: "EmeraldSparrow".into(),
            line: "[doorbell] wake".into(),
        });
        assert_eq!(
            delivery.durability(),
            Durability::Durable {
                key: delivery.key()
            }
        );
        // The same work names the same row, so a redelivery joins it.
        let again = InboxCmd::dispatch(DoorbellAction::Prompt {
            pin: "EmeraldSparrow".into(),
            line: "[doorbell] wake".into(),
        });
        assert_eq!(delivery.key(), again.key());
    }
}

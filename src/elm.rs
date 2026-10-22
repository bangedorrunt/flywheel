//! Framework adapter for the flywheel plane (migrate-flywheel steps 2-4).
//!
//! Stack convention: every plane keeps its framework adapter in a module
//! named `elm` (beads `src/elm`, toron-workflow `engine/elm`, chiebukuro
//! pipeline `elm` modules). `toron_core::kernel` stays the single
//! re-export hub for the vendored crates.
//!
//! `decide` stays pure. Effectful leaves moved here go through the
//! framework's `Interpret` shape: build a description, run it once at the
//! edge. The 4-worker pool and 4096 bound live on the framework executor
//! config, never inside combinators. `resonate-sdk` stays: `durable.rs`
//! still uses it until a `WorkflowEngine` adapter runs the same loop.
//!
//! The framework types come from `toron_core::kernel`, the one module that
//! re-exports the vendored elm-effect crates, so this workspace carries a
//! single framework copy in its lockfile.

pub mod inbox;
pub mod leaf;

use crate::decide::{Snapshot, decide};
use serde::{Deserialize, Serialize};
use toron_core::kernel::{Boot, Cmd as CmdTrait, Durability, Failure, Program, Update};

// The shared journal executor owns the pool construction
// (`effect_journal`, 4 workers and a 4096 bound pinned by its own test);
// this module keeps no second constructor beside it.

/// The classifier as a framework `Program`: the model is the briefing
/// snapshot, a `Classify` message folds into one `DecideCmd`, and the
/// outcome comes back as `Classified`. Nothing here reads the clock, the
/// environment, or the database. Booting a `Loop` around this program is
/// the later `WorkflowEngine` step; until then the binary keeps calling
/// `decide` and the tests below pin the fold they must both agree on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecideMsg {
    Classify { target: String },
    Classified { argv: Vec<String> },
}

/// One classification intent: the full decision surface `cmd_decide`
/// prints, so the binary and the loop agree by construction. Ephemeral:
/// emitting it journals nothing by itself; durability lives in the durable
/// loop that runs the argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecideCmd {
    pub primitive: String,
    pub reason: String,
    pub argv: Vec<String>,
    pub topology: String,
}

impl CmdTrait for DecideCmd {
    fn durability(&self) -> Durability {
        Durability::Ephemeral
    }
}

pub struct DecideProgram;

impl Program for DecideProgram {
    type Model = Snapshot;
    type Msg = DecideMsg;
    type Cmd = DecideCmd;
    type Err = String;

    fn init(_boot: &Boot) -> Update<Snapshot, DecideCmd> {
        Update::quiet(Snapshot::default())
    }

    fn update(model: &Snapshot, msg: &DecideMsg) -> Update<Snapshot, DecideCmd> {
        match msg {
            DecideMsg::Classify { target } => {
                let d = decide(target, model);
                Update {
                    model: model.clone(),
                    commands: vec![DecideCmd {
                        primitive: d.primitive.to_string(),
                        reason: d.reason.clone(),
                        argv: d.argv.clone(),
                        topology: d.topology.to_string(),
                    }],
                }
            }
            DecideMsg::Classified { .. } => Update::quiet(model.clone()),
        }
    }

    fn failure(_failure: &Failure<String>) -> DecideMsg {
        // No outcome is dropped: a non-success outcome re-enters the fold as
        // a fresh classification of the empty target (the default row),
        // which always terminates in one `Classified`.
        DecideMsg::Classify {
            target: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use toron_core::kernel::{
        DurableLog, Effect, FileLog, Interpret, Layer, Loop, ManagedRuntime, MemLog, Producer,
        Runtime, ServiceTag,
    };

    #[derive(Clone)]
    struct NoopSvc;
    struct NoopTag;
    impl ServiceTag for NoopTag {
        type Service = NoopSvc;
        const ID: &'static str = "decide-test-noop";
    }

    fn test_runtime() -> ManagedRuntime {
        let layer = Layer::<NoopTag, std::convert::Infallible, ()>::from_value(NoopSvc);
        ManagedRuntime::make(&layer).expect("value layer builds")
    }

    /// The interpreter (test-only until the `Loop` boot lands): a
    /// classification intent completes as its own outcome message. No process
    /// is spawned here; running the argv stays with the existing durable loop.
    struct DecideInterpret;

    impl Interpret<DecideProgram> for DecideInterpret {
        fn interpret(cmd: &DecideCmd) -> Effect<DecideMsg, String> {
            Effect::succeed(DecideMsg::Classified {
                argv: cmd.argv.clone(),
            })
        }
    }

    #[test]
    fn decide_is_pure_same_input_same_commands() {
        let snap = Snapshot {
            ready_bead_count: 3,
            ..Snapshot::default()
        };
        let msg = DecideMsg::Classify {
            target: "loop work".into(),
        };
        assert_eq!(
            DecideProgram::update(&snap, &msg).commands,
            DecideProgram::update(&snap, &msg).commands,
        );
    }

    #[test]
    fn dropped_command_value_performs_no_io() {
        // Building the fold output must not run it: drop without running.
        let out = DecideProgram::update(
            &Snapshot::default(),
            &DecideMsg::Classify {
                target: "loop work".into(),
            },
        );
        drop(out.commands);
    }

    #[test]
    fn program_fold_matches_decide_on_the_same_snapshot() {
        let snap = Snapshot {
            ready_bead_count: 3,
            ..Snapshot::default()
        };
        let msg = DecideMsg::Classify {
            target: "loop work".into(),
        };
        let first = DecideProgram::update(&snap, &msg);
        let second = DecideProgram::update(&snap, &msg);
        assert_eq!(first.model, snap);
        assert_eq!(first.commands, second.commands);
        let d = decide("loop work", &snap);
        assert_eq!(
            first.commands,
            vec![DecideCmd {
                primitive: d.primitive.to_string(),
                reason: d.reason.clone(),
                argv: d.argv.clone(),
                topology: d.topology.to_string(),
            }]
        );
    }

    #[test]
    fn classified_is_quiet_and_failure_reenters() {
        let snap = Snapshot::default();
        let quiet = DecideProgram::update(
            &snap,
            &DecideMsg::Classified {
                argv: vec!["run".into()],
            },
        );
        assert_eq!(quiet.model, snap);
        assert!(quiet.commands.is_empty());

        let retry = DecideProgram::failure(&Failure::Fail("boom".into()));
        let out = DecideProgram::update(&snap, &retry);
        assert_eq!(out.model, snap);
        assert_eq!(out.commands.len(), 1);
    }

    #[test]
    fn interpreter_completes_the_command_as_its_outcome() {
        let cmd = DecideCmd {
            primitive: "loop".into(),
            reason: "test".into(),
            argv: vec!["loop".into(), "work".into()],
            topology: "swarm".into(),
        };
        let exit = Runtime::run(DecideInterpret::interpret(&cmd));
        assert!(exit.is_success());
        let toron_core::kernel::Exit::Success(msg) = exit else {
            panic!("interpret succeeds");
        };
        assert_eq!(
            msg,
            DecideMsg::Classified {
                argv: vec!["loop".into(), "work".into()]
            }
        );
    }

    #[test]
    fn file_backed_loop_recovers_the_fold_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("flywheel-elm-loop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("loop.jsonl");

        let rt = test_runtime();
        let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path).expect("open log"));
        let mut loop_ = Loop::<DecideProgram, DecideInterpret>::boot(
            &Boot::default(),
            rt,
            log,
            Producer::new("decide-file"),
        )
        .expect("boot runs init");
        loop_
            .send(DecideMsg::Classify {
                target: "loop work".into(),
            })
            .expect("send folds");
        assert!(path.exists(), "the journal reached disk");
        drop(loop_); // Crash: no shutdown, no snapshot.

        let rt = test_runtime();
        let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path).expect("reopen log"));
        let recovered = Loop::<DecideProgram, DecideInterpret>::recover(
            &Boot::default(),
            rt,
            log,
            Producer::new("decide-file"),
        )
        .expect("recover replays the journal");
        assert_eq!(recovered.model(), &Snapshot::default());
        assert!(recovered.in_flight().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_boots_folds_classify_and_settles_quiet() {
        let rt = test_runtime();
        let log: Arc<dyn DurableLog> = Arc::new(MemLog::new());
        let mut loop_ = Loop::<DecideProgram, DecideInterpret>::boot(
            &Boot::default(),
            rt,
            log,
            Producer::new("decide-test"),
        )
        .expect("boot runs init");

        // Init is quiet: the default snapshot, no commands.
        assert_eq!(loop_.model(), &Snapshot::default());
        assert!(loop_.in_flight().is_empty());

        // One Classify folds to one command, the interpreter completes it as
        // Classified, and the fold settles quiet: model unchanged, nothing
        // left in flight, and the run agrees with `decide`.
        loop_
            .send(DecideMsg::Classify {
                target: "loop work".into(),
            })
            .expect("send folds");
        assert_eq!(loop_.model(), &Snapshot::default());
        assert!(loop_.in_flight().is_empty());
        let d = decide("loop work", &Snapshot::default());
        assert_eq!(
            DecideProgram::update(
                &Snapshot::default(),
                &DecideMsg::Classify {
                    target: "loop work".into()
                },
            )
            .commands,
            vec![DecideCmd {
                primitive: d.primitive.to_string(),
                reason: d.reason.clone(),
                argv: d.argv.clone(),
                topology: d.topology.to_string(),
            }]
        );
    }
}

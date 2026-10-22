//! Checkpoint-leaf adapter (durable adapter primitive).
//!
//! Every `#[function]` leaf in `durable.rs` has the same shape: run the
//! self-exe subcommand, parse stdout JSON with a fallback, checkpoint the
//! result so replay returns it without re-running the child. This program
//! is that shape: the fold plans one durable command keyed by the leaf's
//! stable identity, the outcome (or the typed failure) completes it, and a
//! crash between intent and settle redelivers the intent instead of
//! duplicating the effect.
//!
//! The production interpreter runs the same self-exe subprocess through the
//! same bounded runner as the `#[function]` leaves, so there is exactly one
//! implementation of the child wait. Parents wire it via
//! [`run_leaf_journaled`]; scripted interpretation stays in tests.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use toron_core::kernel::{
    Boot, Cmd as CmdTrait, Durability, DurableLog, Effect, Failure, FileLog, Interpret, Loop,
    ManagedRuntime, Producer, Program, Update,
};

/// The leaf fold's model: what to run, and the checkpointed outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafModel {
    pub key: String,
    pub argv: Vec<String>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    /// The defect, when the run hit one (a bug, not the child's answer).
    pub defect: Option<String>,
    /// The run was interrupted (cancelled), so it did not settle.
    pub interrupted: bool,
    pub done: bool,
}

/// Messages into the fold. Outcomes arrive from the provider; nothing here
/// reads the clock, the environment, or the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeafMsg {
    Completed { value: serde_json::Value },
    /// The child's own typed failure (a non-zero exit it reports).
    Failed { error: String },
    /// A defect: a bug in the leaf or the host, not the child's answer.
    Defected { detail: String },
    /// Cancellation. NOT a failure: the run was interrupted, so it must not
    /// settle as one.
    Interrupted,
}

/// One durable intent: run this argv under this key. The key is the leaf's
/// stable identity (e.g. `mail-done:{bead}`), so a retry or a redelivery
/// after a crash joins the same journal row instead of minting a new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafCmd {
    pub key: String,
    pub argv: Vec<String>,
}

impl CmdTrait for LeafCmd {
    fn durability(&self) -> Durability {
        Durability::Durable {
            key: self.key.clone(),
        }
    }
}

pub struct LeafProgram;

impl Program for LeafProgram {
    type Model = LeafModel;
    type Msg = LeafMsg;
    type Cmd = LeafCmd;
    type Err = String;

    fn init(boot: &Boot) -> Update<LeafModel, LeafCmd> {
        let model = LeafModel {
            key: boot.get("key").unwrap_or_default().to_string(),
            argv: boot.args.clone(),
            ..LeafModel::default()
        };
        let cmd = LeafCmd {
            key: model.key.clone(),
            argv: model.argv.clone(),
        };
        Update {
            model,
            commands: vec![cmd],
        }
    }

    fn update(model: &LeafModel, msg: &LeafMsg) -> Update<LeafModel, LeafCmd> {
        if model.done {
            return Update::quiet(model.clone());
        }
        let mut next = model.clone();
        match msg {
            LeafMsg::Completed { value } => {
                next.result = Some(value.clone());
                next.done = true;
            }
            LeafMsg::Failed { error } => {
                next.error = Some(error.clone());
                next.done = true;
            }
            LeafMsg::Defected { detail } => {
                next.defect = Some(detail.clone());
                next.done = true;
            }
            LeafMsg::Interrupted => {
                next.interrupted = true;
                next.done = true;
            }
        }
        Update::quiet(next)
    }

    fn failure(failure: &Failure<String>) -> LeafMsg {
        match failure {
            // The child's own typed failure.
            Failure::Fail(e) => LeafMsg::Failed { error: e.clone() },
            // A defect is a bug, not an answer: it is recorded apart so a
            // caller never reads it as the child having failed.
            Failure::Defect(d) => LeafMsg::Defected {
                detail: format!("{d:?}"),
            },
            // Cancellation is not a failure and must not settle as one.
            Failure::Interrupted => LeafMsg::Interrupted,
            // The durable log refused the append, so the command never ran.
            // That is neither the child's failure nor a defect: the run never
            // happened, so it must not settle as a failed attempt.
            Failure::NotAppended(e) => LeafMsg::Defected {
                detail: format!("the journal refused the append: {e:?}"),
            },
        }
    }
}

/// The production interpreter: run the leaf subprocess exactly the way the
/// `#[function]` leaves do — same self-exe, same bounded runner, same
/// stdout-JSON-with-fallback parse. Nothing here invents a second runner:
/// [`crate::durable::run_leaf_cmd`] is the one implementation, reached
/// through the journal executor's thread hop so a blocking child wait never
/// parks an async worker.
///
/// A non-zero exit or a transport error becomes an in-band `Failed` message
/// (the leaf's historical contract: several leaves always exit 0 and carry
/// `ok` in the payload instead), never a dropped outcome.
pub struct LeafInterpret;

impl Interpret<LeafProgram> for LeafInterpret {
    fn interpret(cmd: &LeafCmd) -> Effect<LeafMsg, String> {
        let cmd = cmd.clone();
        Effect::sync(move || run_leaf_once(&cmd))
    }
}

/// Watchdog label for a leaf subcommand. New leaves fall back to the
/// generic label until they are added here; the label only names timeout
/// errors, it never changes what runs.
fn leaf_label(argv: &[String]) -> &'static str {
    match argv.first().map(String::as_str) {
        Some("__mail-not-ready") => "__mail-not-ready",
        Some("__mail-done") => "__mail-done",
        Some("__reservation-board") => "__reservation-board",
        Some("__read") => "__read",
        Some("__close-check") => "__close-check",
        Some("__reopen-bead") => "__reopen-bead",
        Some("__verify-dispatch") => "__verify-dispatch",
        Some("__inprogress") => "__inprogress",
        Some("__ready") => "__ready",
        Some("__release-bead-locks") => "__release-bead-locks",
        Some("__pin-stamp") => "__pin-stamp",
        _ => "leaf",
    }
}

fn parse_leaf_stdout(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw.trim()).unwrap_or_else(|_| serde_json::json!({ "raw": raw.trim() }))
}

fn run_leaf_once(cmd: &LeafCmd) -> LeafMsg {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return LeafMsg::Failed {
                error: format!("leaf self-exe unavailable: {e}"),
            };
        }
    };
    let mut child = std::process::Command::new(exe);
    child.args(&cmd.argv);
    let label = leaf_label(&cmd.argv);
    let run = crate::durable::run_leaf_cmd(child, label);
    let out = match crate::effect_journal::submit_journal_wait(async move {
        run.await.map_err(|e| anyhow::anyhow!("{e}"))
    }) {
        Ok(out) => out,
        Err(e) => {
            return LeafMsg::Failed {
                error: format!("{label} transport: {e:#}"),
            };
        }
    };
    if !out.status.success() {
        return LeafMsg::Failed {
            error: format!(
                "{label} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        };
    }
    LeafMsg::Completed {
        value: parse_leaf_stdout(&String::from_utf8_lossy(&out.stdout)),
    }
}

/// Journal file for one project's leaves. Pure path computation, so tests
/// can predict and isolate it.
pub fn leaf_journal_path(project: &str) -> std::path::PathBuf {
    crate::state::loops_dir().join(format!("{}.leaf.journal", crate::state::state_key(project)))
}

/// Run one leaf through a journaled loop: boot on the project's leaf
/// journal, run the single durable intent, and return the checkpointed
/// outcome. Semantics match the Resonate leaf this replaces: every call runs
/// the child fresh with one intent journaled per run for the audit trail; a
/// crash mid-run simply re-runs on the next poll, which is the same
/// at-least-once edge the checkpointed leaf had. Recovery without re-running
/// (via [`Loop::recover`]) is available to a future driver that boots once
/// and reuses the loop; this entry point boots per call, so each call is a
/// fresh intent, exactly like each `ctx.run` was a fresh checkpoint.
///
/// A clean run removes its journal file, so the log holds at most one
/// interrupted run: crash residue stays for forensics, completed runs leave
/// nothing behind, and hot paths (every poll) cannot grow the file without
/// bound.
pub fn run_leaf_journaled(
    project: &str,
    subcommand: &str,
    args: &[&str],
    key: &str,
) -> anyhow::Result<serde_json::Value> {
    let journal_path = leaf_journal_path(project);
    if let Some(parent) = journal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    run_leaf_journaled_at(&journal_path, subcommand, args, key)
}

/// The journaled run against an explicit journal file: the seam tests drive
/// (tempdir) instead of the production state dir. The loop boots and drops
/// on a plain OS thread, never the caller's: dropping a tokio runtime
/// inside an async context panics, and the production caller is an async
/// Resonate worker.
pub fn run_leaf_journaled_at(
    journal_path: &std::path::Path,
    subcommand: &str,
    args: &[&str],
    key: &str,
) -> anyhow::Result<serde_json::Value> {
    let path = journal_path.to_path_buf();
    let subcommand = subcommand.to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let key = key.to_string();
    std::thread::spawn(move || {
        let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path)?);
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_leaf_on(log, &subcommand, &args_ref, &key);
        if out.is_ok() {
            let _ = std::fs::remove_file(&path);
        }
        out
    })
    .join()
    .map_err(|_| anyhow::anyhow!("leaf loop thread panicked"))?
}

/// The loop half, over an explicit log: the unit tests drive this with a
/// memory log or a temp-dir file instead of the production journal path.
pub fn run_leaf_on(
    log: Arc<dyn DurableLog>,
    subcommand: &str,
    args: &[&str],
    key: &str,
) -> anyhow::Result<serde_json::Value> {
    let mut argv = vec![subcommand.to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let boot = Boot {
        config: [("key".to_string(), key.to_string())].into_iter().collect(),
        args: argv,
    };
    let loop_ = Loop::<LeafProgram, LeafInterpret>::boot(
        &boot,
        ManagedRuntime::new(),
        log,
        Producer::new(format!("leaf-{key}")),
    )
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let model = loop_.model().clone();
    if let Some(error) = model.error {
        return Err(anyhow::anyhow!("{error}"));
    }
    if let Some(detail) = model.defect {
        return Err(anyhow::anyhow!("leaf defect: {detail}"));
    }
    if model.interrupted {
        return Err(anyhow::anyhow!("leaf run was interrupted"));
    }
    model
        .result
        .ok_or_else(|| anyhow::anyhow!("leaf completed with no result"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::kernel::{
        DurableLog, Effect, FileLog, Interpret, Loop, ManagedRuntime, Producer, ServiceTag,
    };

    #[derive(Clone)]
    struct NoopSvc;
    struct NoopTag;
    impl ServiceTag for NoopTag {
        type Service = NoopSvc;
        const ID: &'static str = "leaf-test-noop";
    }

    fn test_runtime() -> ManagedRuntime {
        let layer =
            toron_core::kernel::Layer::<NoopTag, std::convert::Infallible, ()>::from_value(NoopSvc);
        ManagedRuntime::make(&layer).expect("value layer builds")
    }

    fn test_boot() -> Boot {
        Boot {
            config: [("key".to_string(), "mail-done:bd-1".to_string())]
                .into_iter()
                .collect(),
            args: vec![
                "__mail-done".to_string(),
                "demo".to_string(),
                "bd-1".to_string(),
            ],
        }
    }

    /// Scripted interpreter: the leaf's outcome without a child process.
    struct ScriptedLeaf;
    impl Interpret<LeafProgram> for ScriptedLeaf {
        fn interpret(cmd: &LeafCmd) -> Effect<LeafMsg, String> {
            Effect::succeed(LeafMsg::Completed {
                value: serde_json::json!({ "ok": true, "key": cmd.key }),
            })
        }
    }

    #[test]
    fn init_plans_one_durable_command_under_the_leaf_key() {
        let boot = test_boot();
        let out = LeafProgram::init(&boot);
        assert_eq!(out.commands.len(), 1);
        assert_eq!(out.commands[0].key, "mail-done:bd-1");
        assert_eq!(out.model.key, "mail-done:bd-1");
        assert!(!out.model.done);
    }

    #[test]
    fn completed_and_failed_both_settle_quiet() {
        let boot = test_boot();
        let model = LeafProgram::init(&boot).model;
        let done = LeafProgram::update(
            &model,
            &LeafMsg::Completed {
                value: serde_json::json!({ "ok": true }),
            },
        );
        assert!(done.model.done);
        assert!(done.commands.is_empty());
        assert!(done.model.error.is_none());

        let failed = LeafProgram::update(
            &model,
            &LeafMsg::Failed {
                error: "timeout".into(),
            },
        );
        assert!(failed.model.done);
        assert_eq!(failed.model.error.as_deref(), Some("timeout"));

        // A second outcome after done changes nothing: no double settle.
        let again = LeafProgram::update(
            &failed.model,
            &LeafMsg::Completed {
                value: serde_json::json!({ "ok": true }),
            },
        );
        assert_eq!(again.model, failed.model);
        assert!(again.commands.is_empty());
    }

    #[test]
    fn failure_maps_to_failed_never_a_dropped_outcome() {
        let msg = LeafProgram::failure(&Failure::Fail("boom".into()));
        assert!(matches!(msg, LeafMsg::Failed { .. }));
    }

    #[test]
    fn file_backed_leaf_recovers_its_checkpoint_without_rerunning() {
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("flywheel-elm-leaf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("leaf.journal");

        let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path).expect("open log"));
        let mut loop_ = Loop::<LeafProgram, ScriptedLeaf>::boot(
            &test_boot(),
            test_runtime(),
            log,
            Producer::new("leaf-test"),
        )
        .expect("boot runs init");
        // Init planned the command; send nothing: the drain runs it.
        assert_eq!(loop_.model().key, "mail-done:bd-1");
        drop(loop_); // Crash: no shutdown, no snapshot.

        let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path).expect("reopen log"));
        let recovered = Loop::<LeafProgram, ScriptedLeaf>::recover(
            &test_boot(),
            test_runtime(),
            log,
            Producer::new("leaf-test"),
        )
        .expect("recover replays the journal");
        // The init command was durable and already completed before the
        // crash: recovery replays the journal without re-running it, and
        // the checkpoint holds.
        assert!(recovered.in_flight().is_empty());
        assert_eq!(
            recovered.model().result,
            Some(serde_json::json!({ "ok": true, "key": "mail-done:bd-1" }))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaf_label_names_known_leaves_and_falls_back() {
        assert_eq!(leaf_label(&["__mail-done".to_string()]), "__mail-done");
        assert_eq!(leaf_label(&["__nope".to_string()]), "leaf");
        assert_eq!(leaf_label(&[]), "leaf");
    }

    #[test]
    fn parse_leaf_stdout_prefers_json_and_keeps_raw() {
        assert_eq!(
            parse_leaf_stdout("{\"ok\":true}\n"),
            serde_json::json!({ "ok": true })
        );
        assert_eq!(
            parse_leaf_stdout("not json\n"),
            serde_json::json!({ "raw": "not json" })
        );
    }

    #[test]
    fn production_interpret_runs_the_child_and_parses() {
        // The test harness binary answers `--help` with exit 0 and
        // non-JSON stdout, exercising the real spawn plus the fallback
        // parse without touching any loop state.
        let cmd = LeafCmd {
            key: "leaf-test:help".into(),
            argv: vec!["--help".into()],
        };
        let msg = run_leaf_once(&cmd);
        match msg {
            LeafMsg::Completed { value } => {
                assert!(value.get("raw").is_some(), "{value}");
            }
            LeafMsg::Failed { error } => panic!("self-exe --help failed: {error}"),
            LeafMsg::Defected { detail } => panic!("self-exe --help hit a defect: {detail}"),
            LeafMsg::Interrupted => panic!("self-exe --help was interrupted"),
        }
    }

    /// A defect is a bug and an interruption is cancellation: neither is the
    /// child's typed failure, and the fold must keep them apart (a cancelled
    /// run that settles as a failed attempt is a lie the ledger repeats).
    #[test]
    fn a_defect_and_an_interruption_are_not_the_child_s_typed_failure() {
        let model = LeafModel::default();

        let defect = LeafProgram::update(
            &model,
            &LeafMsg::Defected {
                detail: "boom".into(),
            },
        );
        assert!(
            defect.model.defect.is_some(),
            "a defect is recorded as a defect"
        );
        assert!(
            defect.model.error.is_none(),
            "a defect must not settle as the child's typed failure"
        );

        let interrupted = LeafProgram::update(&model, &LeafMsg::Interrupted);
        assert!(
            interrupted.model.interrupted,
            "an interruption is recorded as an interruption"
        );
        assert!(
            interrupted.model.error.is_none(),
            "cancellation must not settle as a typed failure"
        );

        // The framework's own arms map to the same three messages.
        assert!(matches!(
            LeafProgram::failure(&Failure::Interrupted),
            LeafMsg::Interrupted
        ));
        assert!(matches!(
            LeafProgram::failure(&Failure::Fail("boom".to_string())),
            LeafMsg::Failed { .. }
        ));
    }

    #[test]
    fn run_leaf_on_drives_the_full_loop_without_a_journal_path() {
        // End to end through `run_leaf_on`: boot, durable intent, real
        // child, checkpointed outcome — over a memory log, so no state dir
        // is touched.
        use toron_core::kernel::MemLog;

        let log: Arc<dyn DurableLog> = Arc::new(MemLog::new());
        let out = run_leaf_on(log, "--help", &[], "leaf-test:e2e").expect("leaf runs");
        assert!(out.get("raw").is_some(), "{out}");
    }

    #[test]
    fn run_leaf_journaled_at_truncates_on_success() {
        // A clean run leaves no journal behind: crash residue is the only
        // thing worth keeping, so hot paths cannot grow the file.
        let dir =
            std::env::temp_dir().join(format!("flywheel-leaf-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("leaf.journal");
        let out =
            run_leaf_journaled_at(&path, "--help", &[], "leaf-test:truncate").expect("leaf runs");
        assert!(out.get("raw").is_some(), "{out}");
        assert!(!path.exists(), "a clean run removes its journal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaf_journal_path_stays_under_the_state_loops_dir() {
        let path = leaf_journal_path("demo");
        assert_eq!(
            path.parent(),
            Some(crate::state::loops_dir().as_path()),
            "{path:?}"
        );
        assert!(
            path.extension().and_then(|e| e.to_str()) == Some("journal"),
            "{path:?}"
        );
    }
}

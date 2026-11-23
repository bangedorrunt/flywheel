//! Background proof train (bd-7qhz0).
//!
//! The train is a pure reducer over `(Model, Msg)`. Every external effect is a
//! `Cmd` value; the driver journals a command BEFORE handing it to a `Runner`.
//! Tests use a fake clock and a fake runner, so no test waits on a proof.
//!
//! Rules: prove the newest HEAD once the previous run ends and HEAD moved;
//! keep only the newest green receipt; red runs page only unproven rows' owners
//! and the train carries on; landing picks the newest green receipt covering a
//! bead (see `cmd::ship`).

use std::collections::BTreeSet;

/// A proof run in flight.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Running {
    pub sha: String,
    pub started_ms: u64,
    pub step: String,
}

/// A finished proof, green or red.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finished {
    pub sha: String,
    pub ok: bool,
    pub receipt: String,
    pub finished_ms: u64,
}

/// The one model that owns train facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub running: Option<Running>,
    pub last_green: Option<Finished>,
    pub last_red: Option<Finished>,
    pub seen_head: Option<String>,
    pub proved: BTreeSet<String>,
    pub effects: Vec<String>,
}

/// Inputs, including time — the reducer never reads a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Poll HEAD. `head` is supplied by the port, not read by the reducer.
    Poll { now_ms: u64, head: String },
    /// A proof run opened; the port names the step it entered.
    Started { now_ms: u64, step: String },
    /// The driver moved to another proof step.
    Stepped { step: String },
    /// The proof ended. `receipt` is the path recorded by the driver.
    Finished {
        now_ms: u64,
        ok: bool,
        receipt: String,
        unproven: Vec<String>,
    },
}

/// Effect values. The driver journals each before running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    StartProof { sha: String },
    MailUnproven { rows: Vec<String> },
}

/// Pure tick. No I/O, clock, env, randomness or locks.
pub fn tick(model: &Model, msg: Msg) -> (Model, Vec<Cmd>) {
    let mut m = model.clone();
    let mut cmds = Vec::new();
    match msg {
        Msg::Poll { now_ms, head } => {
            m.seen_head = Some(head.clone());
            let already =
                m.running.as_ref().is_some_and(|r| r.sha == head) || m.proved.contains(&head);
            if m.running.is_none() && !already {
                m.running = Some(Running {
                    sha: head.clone(),
                    started_ms: now_ms,
                    step: "queued".to_string(),
                });
                cmds.push(Cmd::StartProof { sha: head });
            }
        }
        Msg::Started { now_ms, step } => {
            if let Some(r) = m.running.as_mut() {
                r.started_ms = now_ms;
                r.step = step;
            }
        }
        Msg::Stepped { step } => {
            if let Some(r) = m.running.as_mut() {
                r.step = step;
            }
        }
        Msg::Finished {
            now_ms,
            ok,
            receipt,
            unproven,
        } => {
            let Some(r) = m.running.take() else {
                return (m, cmds);
            };
            let done = Finished {
                sha: r.sha.clone(),
                ok,
                receipt,
                finished_ms: now_ms,
            };
            m.proved.insert(r.sha.clone());
            if ok {
                m.last_green = Some(done);
            } else {
                m.last_red = Some(done);
                if !unproven.is_empty() {
                    cmds.push(Cmd::MailUnproven { rows: unproven });
                }
            }
            // A newer HEAD that arrived during the run starts the next one.
            if let Some(head) = m.seen_head.clone()
                && head != r.sha
                && !m.proved.contains(&head)
            {
                m.running = Some(Running {
                    sha: head.clone(),
                    started_ms: now_ms,
                    step: "queued".to_string(),
                });
                cmds.push(Cmd::StartProof { sha: head });
            }
        }
    }
    (m, cmds)
}

/// Status projection (`flywheel proof status`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub running_sha: Option<String>,
    pub step: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub last_green_sha: Option<String>,
    pub last_red_sha: Option<String>,
    pub proved: Vec<String>,
}

pub fn status(model: &Model, now_ms: u64) -> Status {
    Status {
        running_sha: model.running.as_ref().map(|r| r.sha.clone()),
        step: model.running.as_ref().map(|r| r.step.clone()),
        elapsed_ms: model
            .running
            .as_ref()
            .map(|r| now_ms.saturating_sub(r.started_ms)),
        last_green_sha: model.last_green.as_ref().map(|f| f.sha.clone()),
        last_red_sha: model.last_red.as_ref().map(|f| f.sha.clone()),
        proved: model.proved.iter().cloned().collect(),
    }
}

/// Ports used by the interpreter. Tests implement these with fake data.
pub trait Runner {
    fn now_ms(&self) -> u64;
    fn head(&self) -> String;
    fn run_proof(&mut self, sha: &str) -> (bool, String, Vec<String>);
    fn mail(&mut self, rows: &[String]);
    fn journal(&mut self, cmd: &Cmd);
}

/// One driver step: poll HEAD, then perform every command the tick emitted,
/// journaling each before its provider runs and feeding the result back into
/// the reducer. The model is the single owner of train facts.
pub fn step<C: Runner>(model: &mut Model, runner: &mut C) {
    let head = runner.head();
    let (next, cmds) = tick(
        model,
        Msg::Poll {
            now_ms: runner.now_ms(),
            head,
        },
    );
    *model = next;
    for cmd in cmds {
        runner.journal(&cmd);
        match cmd {
            Cmd::StartProof { sha } => {
                let (after, _started) = tick(
                    model,
                    Msg::Started {
                        now_ms: runner.now_ms(),
                        step: "proof".to_string(),
                    },
                );
                *model = after;
                let (ok, receipt, unproven) = runner.run_proof(&sha);
                let (after, more) = tick(
                    model,
                    Msg::Stepped {
                        step: "proof".to_string(),
                    },
                );
                *model = after;
                let (after, more_done) = tick(
                    model,
                    Msg::Finished {
                        now_ms: runner.now_ms(),
                        ok,
                        receipt,
                        unproven,
                    },
                );
                *model = after;
                for extra in more.into_iter().chain(more_done) {
                    runner.journal(&extra);
                    if let Cmd::MailUnproven { rows } = extra {
                        runner.mail(&rows);
                    }
                }
            }
            Cmd::MailUnproven { rows } => runner.mail(&rows),
        }
    }
}

/// Where the train's persisted model lives. One file, rewritten atomically.
pub fn state_path() -> std::path::PathBuf {
    crate::config::state_dir().join("proof-train.json")
}

pub fn load_model() -> Model {
    let Ok(raw) = std::fs::read_to_string(state_path()) else {
        return Model::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_model(model: &Model) -> anyhow::Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(model)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// The live port: HEAD from git, proof from the nightly sweep script (which
/// calls clean-proof as a black box), mail through toron.
struct LiveRunner;

impl Runner for LiveRunner {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    fn head(&self) -> String {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
    fn run_proof(&mut self, _sha: &str) -> (bool, String, Vec<String>) {
        let out = std::process::Command::new("bash")
            .arg("scripts/nightly-mutation-sweep.sh")
            .output();
        match out {
            Ok(o) if o.status.success() => (true, "nightly sweep: green".into(), vec![]),
            Ok(o) => (
                false,
                "nightly sweep: red".into(),
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| l.contains("survivor"))
                    .map(str::to_string)
                    .collect(),
            ),
            Err(e) => (false, format!("nightly sweep: {e}"), vec![]),
        }
    }
    fn mail(&mut self, rows: &[String]) {
        let body = rows.join("\n");
        let _ = std::process::Command::new("toron")
            .args([
                "mail",
                "send",
                "--project",
                "flywheel",
                "--as",
                "TealCove",
                "--to",
                "SlateHarbor",
                "--subject",
                "[proof train] unproven mutations",
                "--body",
                &body,
            ])
            .status();
    }
    fn journal(&mut self, cmd: &Cmd) {
        eprintln!("flywheel proof train: journal {cmd:?}");
    }
}

pub fn cmd_once(json: bool) -> anyhow::Result<()> {
    let mut model = load_model();
    let mut runner = LiveRunner;
    step(&mut model, &mut runner);
    save_model(&model)?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&status(&model, runner.now_ms()))?
        );
    } else {
        let s = status(&model, runner.now_ms());
        println!(
            "running={} step={} last_green={}",
            s.running_sha.as_deref().unwrap_or("-"),
            s.step.as_deref().unwrap_or("-"),
            s.last_green_sha.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

pub fn cmd_status(json: bool) -> anyhow::Result<()> {
    let model = load_model();
    let now = LiveRunner.now_ms();
    let s = status(&model, now);
    if json {
        println!("{}", serde_json::to_string(&s)?);
    } else {
        println!(
            "running={} step={} elapsed_ms={} last_green={} last_red={} covered={:?}",
            s.running_sha.as_deref().unwrap_or("-"),
            s.step.as_deref().unwrap_or("-"),
            s.elapsed_ms.unwrap_or(0),
            s.last_green_sha.as_deref().unwrap_or("-"),
            s.last_red_sha.as_deref().unwrap_or("-"),
            s.proved
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll(m: &Model, now: u64, head: &str) -> (Model, Vec<Cmd>) {
        tick(
            m,
            Msg::Poll {
                now_ms: now,
                head: head.to_string(),
            },
        )
    }

    #[test]
    fn a_poll_queues_one_proof_for_a_new_head() {
        let (m, cmds) = poll(&Model::default(), 100, "aaa");
        assert_eq!(cmds, vec![Cmd::StartProof { sha: "aaa".into() }]);
        assert_eq!(m.running.as_ref().unwrap().sha, "aaa");
        let (m2, cmds2) = poll(&m, 200, "aaa");
        assert!(cmds2.is_empty(), "same head must not restart a run");
        assert_eq!(m2, m);
    }

    #[test]
    fn a_new_head_during_a_run_starts_after_it_ends() {
        let (running, _) = poll(&Model::default(), 100, "aaa");
        let (seen, _) = poll(&running, 150, "bbb");
        assert_eq!(seen.seen_head.as_deref(), Some("bbb"));
        assert_eq!(seen.running.as_ref().unwrap().sha, "aaa");
        let (idle, cmds) = tick(
            &seen,
            Msg::Finished {
                now_ms: 200,
                ok: true,
                receipt: "/r/aaa.json".into(),
                unproven: vec![],
            },
        );
        assert!(cmds.contains(&Cmd::StartProof { sha: "bbb".into() }));
        assert_eq!(idle.running.as_ref().unwrap().sha, "bbb");
        assert_eq!(idle.last_green.as_ref().unwrap().sha, "aaa");
    }

    #[test]
    fn red_mails_only_unproven_rows_and_keeps_the_last_green() {
        let (running, _) = poll(&Model::default(), 100, "aaa");
        let (m, cmds) = tick(
            &running,
            Msg::Finished {
                now_ms: 200,
                ok: false,
                receipt: "/r/aaa-red.json".into(),
                unproven: vec!["VACUOUS spec#M1".into()],
            },
        );
        assert_eq!(
            cmds,
            vec![Cmd::MailUnproven {
                rows: vec!["VACUOUS spec#M1".into()]
            }]
        );
        assert_eq!(m.last_red.as_ref().unwrap().sha, "aaa");
        assert!(m.last_green.is_none());
    }

    #[test]
    fn status_uses_the_fake_clock_not_ambient_time() {
        let (m, _) = poll(&Model::default(), 1_000, "aaa");
        let s = status(&m, 1_250);
        assert_eq!(s.elapsed_ms, Some(250));
        assert_eq!(s.running_sha.as_deref(), Some("aaa"));
        assert_eq!(s.step.as_deref(), Some("queued"));
    }

    #[test]
    fn a_finished_message_without_a_run_is_a_no_op() {
        let (m, cmds) = tick(
            &Model::default(),
            Msg::Finished {
                now_ms: 1,
                ok: true,
                receipt: "x".into(),
                unproven: vec![],
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(m, Model::default());
    }

    struct Fake {
        head: String,
        ok: bool,
        journal: Vec<String>,
        mailed: Vec<Vec<String>>,
    }

    impl Runner for Fake {
        fn now_ms(&self) -> u64 {
            1_000
        }
        fn head(&self) -> String {
            self.head.clone()
        }
        fn run_proof(&mut self, _sha: &str) -> (bool, String, Vec<String>) {
            let rows = if self.ok {
                vec![]
            } else {
                vec!["VACUOUS spec#M1".to_string()]
            };
            (self.ok, "/r.json".into(), rows)
        }
        fn mail(&mut self, rows: &[String]) {
            self.mailed.push(rows.to_vec());
        }
        fn journal(&mut self, cmd: &Cmd) {
            self.journal.push(format!("{cmd:?}"));
        }
    }

    #[test]
    fn the_driver_journals_before_the_provider_runs() {
        let mut fake = Fake {
            head: "aaa".into(),
            ok: true,
            journal: vec![],
            mailed: vec![],
        };
        let mut model = Model::default();
        step(&mut model, &mut fake);
        assert_eq!(fake.journal.len(), 1, "one journal row per effect");
        assert!(fake.journal[0].contains("StartProof"));
        assert!(model.running.is_none(), "the run is finished");
        assert!(model.proved.contains("aaa"));
        assert!(model.last_green.is_some());
    }

    #[test]
    fn a_red_run_mails_owners_after_the_journal_row() {
        let mut fake = Fake {
            head: "aaa".into(),
            ok: false,
            journal: vec![],
            mailed: vec![],
        };
        let mut model = Model::default();
        step(&mut model, &mut fake);
        assert!(fake.journal[0].contains("StartProof"));
        assert!(fake.journal[1].contains("MailUnproven"));
        assert_eq!(fake.mailed.len(), 1, "unproven rows are mailed");
        assert!(model.last_red.is_some());
    }
}

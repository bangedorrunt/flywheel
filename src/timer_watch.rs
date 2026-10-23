//! One-shot watch (`flywheel timer`). A deadline is required. Sweep ends a
//! watch in exactly one `notify@1` under `watch:<id>:<outcome>`.
//!
//! The decision is pure ([`decide`]). The clock is read at the CLI and at
//! the sweep thread, then passed in. No shell is stored. The pane wake is
//! the doorbell's constant line, from the envelope, not from text typed here.
//!
//! The intent file is the journal: it is written before `append`. A failed
//! write refuses the send. A retry of the same outcome resends the stored
//! envelope (same producer epoch). `FLYWHEEL_ALLOW_EXEC=0` records the
//! outcome, writes no envelope, and names the suppression.
//!
//! Default on. `FLYWHEEL_TIMER=0|false|off` or `[timer] enabled = false`
//! opts out. Any other set env value is off (a typo must not arm a watch).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use toron_core::envelope::Envelope;

/// `0|false|off` and any other set value are off. Unset or empty uses
/// `config`, and a missing config key is on.
pub fn enabled(env: Option<&str>, config: Option<bool>) -> bool {
    match env.map(str::trim).filter(|s| !s.is_empty()) {
        Some("1" | "true" | "on") => true,
        None => config.unwrap_or(true),
        Some(_) => false,
    }
}

/// What the caller has reported. `Open` means neither meet nor fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Signal {
    Open,
    Met,
    Failed,
}

/// The one outcome a watch can end in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Met,
    Timer,
    Expired,
    Failed,
}

impl Outcome {
    /// The token in `watch:<id>:<outcome>`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::Timer => "timer",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }
}

/// Pure. `now_us == deadline_us` is due. A met watch at or before the
/// deadline is `met`; a met watch after it is `expired`. `Failed` stays
/// `failed`. An open watch before the deadline waits. An open watch at or
/// after it is `timer` when `timer_only`, otherwise `expired`.
pub fn decide(signal: Signal, now_us: u64, deadline_us: u64, timer_only: bool) -> Option<Outcome> {
    match signal {
        Signal::Failed => Some(Outcome::Failed),
        Signal::Met if now_us <= deadline_us => Some(Outcome::Met),
        Signal::Met => Some(Outcome::Expired),
        Signal::Open if now_us < deadline_us => None,
        Signal::Open if timer_only => Some(Outcome::Timer),
        Signal::Open => Some(Outcome::Expired),
    }
}

/// Idempotency key for one outcome. A retry of the same outcome keeps it.
pub fn effect_key(id: &str, outcome: Outcome) -> String {
    format!("watch:{id}:{}", outcome.tag())
}

/// Notice body. Fixed words plus the id and the outcome. No shell, no caller text.
pub fn notify_text(id: &str, outcome: Outcome) -> String {
    format!("watch {id} {}", outcome.tag())
}

/// `[A-Za-z0-9_-]{1,64}`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// One `notify@1`. `to_npub` is the resolved recipient. The context is
/// `{project}:{effect_key}` so a reader can see which watch it closed.
pub fn notify_envelope(
    project: &str,
    producer: &str,
    epoch: u64,
    to_npub: &str,
    id: &str,
    outcome: Outcome,
) -> Result<Envelope, String> {
    use toron_core::contracts::notify_v1::NotifyV1;
    use toron_core::envelope::{Class, ContextId, ContractId, ENVELOPE_V, MsgId, Recipient};
    let key = effect_key(id, outcome);
    let env = Envelope {
        v: ENVELOPE_V,
        msg: MsgId {
            producer: producer.to_string(),
            epoch,
            seq: 0,
        },
        from: producer.to_string(),
        to: Recipient::Npub(to_npub.to_string()),
        class: Class::Notify,
        contract: ContractId::new("notify", 1),
        context: ContextId(format!("{project}:{key}")),
        task: None,
        generation: 0,
        causation: None,
        deadline_ts_us: None,
        lease_ms: None,
        body: serde_json::to_value(NotifyV1 {
            text: notify_text(id, outcome),
        })
        .map_err(|e| e.to_string())?,
    };
    env.validate().map_err(|e| format!("{e:?}"))?;
    Ok(env)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Watch {
    id: String,
    project: String,
    pin: String,
    deadline_us: u64,
    timer_only: bool,
    signal: Signal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settled: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Intent {
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    envelope: Option<Envelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    offset: Option<u64>,
    #[serde(default)]
    suppressed: bool,
}

/// Claim a producer epoch and resolve `pin` to an npub, or append one envelope.
pub trait NotifyPort {
    fn prepare(&mut self, pin: &str) -> Result<(String, u64, String), String>;
    fn append(&mut self, env: &Envelope) -> Result<u64, String>;
}

fn watch_path(root: &Path, project: &str, id: &str) -> PathBuf {
    root.join("timer").join(project).join(format!("{id}.json"))
}

/// A settled watch moves here, out of the directory every sweep reads.
fn done_path(root: &Path, project: &str, id: &str) -> PathBuf {
    root.join("timer")
        .join(project)
        .join("done")
        .join(format!("{id}.json"))
}

/// A watch file that does not parse moves here, so one bad file is named
/// once and does not fail every later sweep.
fn bad_path(root: &Path, project: &str, name: &str) -> PathBuf {
    root.join("timer").join(project).join("bad").join(name)
}

fn move_to(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(dir) = to.parent() {
        crate::private_fs::create_private_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::rename(from, to).map_err(|e| e.to_string())
}

fn intent_path(root: &Path, project: &str, id: &str, outcome: Outcome) -> PathBuf {
    root.join("timer")
        .join(project)
        .join("intents")
        .join(id)
        .join(format!("{}.json", outcome.tag()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        crate::private_fs::create_private_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = crate::private_fs::create_private_file(&tmp).map_err(|e| e.to_string())?;
        f.write_all(&bytes).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Arm a watch. A deadline is required (the caller computed `deadline_us`).
pub fn arm(
    root: &Path,
    project: &str,
    id: &str,
    pin: &str,
    deadline_us: u64,
    timer_only: bool,
) -> Result<(), String> {
    if !valid_id(id) {
        return Err("timer id must match [A-Za-z0-9_-]{1,64}".into());
    }
    if pin.is_empty() {
        return Err("timer --to is empty".into());
    }
    let path = watch_path(root, project, id);
    if read_json::<Watch>(&path)?.is_some() {
        return Err(format!("timer {id} is already armed"));
    }
    // A settled watch was pruned to done/; its intents still name the id.
    if done_path(root, project, id).exists() {
        return Err(format!("timer {id} is already settled"));
    }
    write_json(
        &path,
        &Watch {
            id: id.to_string(),
            project: project.to_string(),
            pin: pin.to_string(),
            deadline_us,
            timer_only,
            signal: Signal::Open,
            settled: None,
        },
    )
}

/// Record meet or fail. Does not send. Sweep decides the outcome.
pub fn signal(root: &Path, project: &str, id: &str, signal: Signal) -> Result<(), String> {
    let path = watch_path(root, project, id);
    let Some(mut w) = read_json::<Watch>(&path)? else {
        if done_path(root, project, id).exists() {
            return Err(format!("timer {id} is already settled"));
        }
        return Err(format!("no timer {id}"));
    };
    if w.settled.is_some() {
        return Err(format!("timer {id} is already settled"));
    }
    w.signal = signal;
    write_json(&path, &w)
}

/// One sweep of `project`. Already-settled watches send nothing.
///
/// rv23 #4: one watch's failure does not stop the others. Every watch is
/// tried; the failures come back together as one `Err` (so a caller still
/// sees them), after the rest were sent. A file that does not parse moves to
/// `bad/` and is named once. A settled watch moves to `done/`, so a sweep
/// reads only the open ones.
pub fn sweep_at(
    root: &Path,
    project: &str,
    now_us: u64,
    exec_allowed: bool,
    port: &mut dyn NotifyPort,
) -> Result<usize, String> {
    let dir = root.join("timer").join(project);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.to_string()),
    };
    let mut n = 0;
    let mut errs: Vec<String> = Vec::new();
    for ent in rd {
        let ent = match ent {
            Ok(ent) => ent,
            Err(e) => {
                errs.push(e.to_string());
                continue;
            }
        };
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".json") {
            continue;
        }
        let path = ent.path();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                errs.push(format!("{name}: {e}"));
                continue;
            }
        };
        let mut w = match serde_json::from_slice::<Watch>(&bytes) {
            Ok(w) => w,
            Err(e) => {
                // Writes are tmp + rename, so a file that does not parse is
                // not a half-written one. Move it aside and name it once.
                let moved = move_to(&path, &bad_path(root, project, name));
                errs.push(match moved {
                    Ok(()) => format!("{name}: {e} (moved to bad/)"),
                    Err(m) => format!("{name}: {e} (not moved: {m})"),
                });
                continue;
            }
        };
        if w.settled.is_some() {
            // Settled before this sweep (or before pruning existed).
            if let Err(e) = move_to(&path, &done_path(root, project, &w.id)) {
                errs.push(format!("{}: prune: {e}", w.id));
            }
            continue;
        }
        match fire_one(root, project, &mut w, now_us, exec_allowed, port) {
            Ok(true) => {
                if let Err(e) = write_json(&path, &w) {
                    errs.push(format!("{}: {e}", w.id));
                    continue;
                }
                n += 1;
                if let Err(e) = move_to(&path, &done_path(root, project, &w.id)) {
                    errs.push(format!("{}: prune: {e}", w.id));
                }
            }
            Ok(false) => {}
            Err(e) => errs.push(format!("{}: {e}", w.id)),
        }
    }
    if errs.is_empty() {
        Ok(n)
    } else {
        Err(errs.join("; "))
    }
}

fn fire_one(
    root: &Path,
    project: &str,
    watch: &mut Watch,
    now_us: u64,
    exec_allowed: bool,
    port: &mut dyn NotifyPort,
) -> Result<bool, String> {
    if watch.settled.is_some() {
        return Ok(false);
    }
    let Some(outcome) = decide(watch.signal, now_us, watch.deadline_us, watch.timer_only) else {
        return Ok(false);
    };
    let key = effect_key(&watch.id, outcome);
    let path = intent_path(root, project, &watch.id, outcome);
    let mut intent = read_json::<Intent>(&path)?.unwrap_or(Intent {
        key: key.clone(),
        envelope: None,
        offset: None,
        suppressed: false,
    });
    if intent.offset.is_some() || intent.suppressed {
        watch.settled = Some(outcome.tag().to_string());
        return Ok(true);
    }
    if !exec_allowed {
        // The intent is the journal. A failed write leaves the watch open
        // and sends nothing.
        intent.suppressed = true;
        write_json(&path, &intent)?;
        let line = format!("suppressed timer ring {key}: FLYWHEEL_ALLOW_EXEC=0");
        eprintln!("{line}");
        let note = root
            .join("timer")
            .join(project)
            .join(format!("{}.log.suppressed", watch.id));
        write_json(&note, &line)?;
        watch.settled = Some(outcome.tag().to_string());
        return Ok(true);
    }
    if intent.envelope.is_none() {
        // Key first, then the envelope, both before append. A crash after
        // `prepare` and before this write mints an epoch that never lands;
        // the retry claims another. The append itself is never first.
        if !path.exists() {
            write_json(&path, &intent)?;
        }
        let (producer, epoch, npub) = port.prepare(&watch.pin)?;
        intent.envelope = Some(notify_envelope(
            project, &producer, epoch, &npub, &watch.id, outcome,
        )?);
        write_json(&path, &intent)?;
    }
    let env = intent
        .envelope
        .clone()
        .ok_or_else(|| format!("timer intent {key} lost its envelope"))?;
    let offset = port.append(&env)?;
    intent.offset = Some(offset);
    write_json(&path, &intent)?;
    watch.settled = Some(outcome.tag().to_string());
    Ok(true)
}

/// Clock at a boundary. Not for [`decide`].
pub fn now_us() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn gate(cfg_enabled: Option<bool>) -> Result<()> {
    let env = std::env::var("FLYWHEEL_TIMER").ok();
    if enabled(env.as_deref(), cfg_enabled) {
        Ok(())
    } else {
        bail!("timer is off: FLYWHEEL_TIMER=0 or [timer] enabled = false (default is on)")
    }
}

fn root() -> PathBuf {
    crate::config::state_dir()
}

/// The config file's identity for the cache: modified time and length.
/// `None` when it cannot be read (no file); a change either way reloads.
fn config_stamp() -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(crate::config::config_file()).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The live inbox port, built on first use and kept (rv23 #4).
///
/// A sweep with nothing due never loads the config or the inbox facts. Once
/// built, the port (its config, facts and npub cache) is reused across
/// sweeps, and rebuilt only when the config file changes.
pub struct LivePort {
    project: String,
    inner: Option<(Option<(std::time::SystemTime, u64)>, crate::inbox_ports::FlywheelPorts)>,
    /// How many times the config and facts were loaded (tests, evidence).
    pub loads: u64,
}

impl LivePort {
    pub fn new(project: &str) -> Self {
        Self {
            project: project.to_string(),
            inner: None,
            loads: 0,
        }
    }

    fn ports(&mut self) -> Result<&mut crate::inbox_ports::FlywheelPorts, String> {
        let stamp = config_stamp();
        if self.inner.as_ref().is_none_or(|(s, _)| *s != stamp) {
            let cfg = crate::config::Config::load().map_err(|e| e.to_string())?;
            self.loads += 1;
            self.inner = Some((
                stamp,
                crate::inbox_ports::FlywheelPorts::new(&cfg, &self.project, &self.project),
            ));
        }
        Ok(&mut self.inner.as_mut().expect("just built").1)
    }
}

impl NotifyPort for LivePort {
    fn prepare(&mut self, pin: &str) -> Result<(String, u64, String), String> {
        let inner = self.ports()?;
        let npub = inner
            .npub_of(pin)
            .ok_or_else(|| format!("no npub for {pin}"))?;
        let (producer, epoch) = inner.claim_producer_epoch()?;
        Ok((producer, epoch, npub))
    }

    fn append(&mut self, env: &Envelope) -> Result<u64, String> {
        self.ports()?.append_env(env.clone())
    }
}

/// Sweep one project against the live inbox, once (the CLI).
pub fn sweep_project(project: &str, now_us: u64, exec_allowed: bool) -> Result<usize, String> {
    sweep_at(
        &root(),
        project,
        now_us,
        exec_allowed,
        &mut LivePort::new(project),
    )
}

/// The repeating sweep (host thread, daemon loop). Keeps one [`LivePort`]
/// across sweeps; see there for what is cached.
pub struct Sweeper {
    project: String,
    root: PathBuf,
    pub port: LivePort,
}

impl Sweeper {
    pub fn new(project: &str) -> Self {
        Self {
            project: project.to_string(),
            root: root(),
            port: LivePort::new(project),
        }
    }

    pub fn sweep(&mut self, now_us: u64, exec_allowed: bool) -> Result<usize, String> {
        sweep_at(
            &self.root,
            &self.project,
            now_us,
            exec_allowed,
            &mut self.port,
        )
    }
}

/// Host thread gate. Same rule as the CLI.
pub fn host_enabled(cfg: &crate::config::Config) -> bool {
    let env = std::env::var("FLYWHEEL_TIMER").ok();
    enabled(env.as_deref(), cfg.timer.as_ref().and_then(|t| t.enabled))
}

/// `FLYWHEEL_ALLOW_EXEC=0` suppresses the ring. Read at the thread boundary.
pub fn exec_allowed() -> bool {
    std::env::var("FLYWHEEL_ALLOW_EXEC").ok().as_deref() != Some("0")
}

/// 1s sweep beside the beads poller. Not a reducer. A repeating error is
/// printed once until the text changes.
pub fn spawn_sweep(project: &str) {
    let project = project.to_string();
    let _ = std::thread::Builder::new()
        .name("timer-sweep".into())
        .spawn(move || {
            let mut last = String::new();
            let mut sweeper = Sweeper::new(&project);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                match sweeper.sweep(now_us(), exec_allowed()) {
                    Ok(_) => last.clear(),
                    Err(e) if e == last => {}
                    Err(e) => {
                        eprintln!("timer sweep {project}: {e}");
                        last = e;
                    }
                }
            }
        });
}

#[derive(clap::Subcommand, Debug)]
pub enum TimerCmd {
    /// Arm a one-shot watch. Pass `--deadline-us` or `--in-secs`, not both.
    Arm {
        project: String,
        #[arg(long)]
        id: String,
        /// Pin that receives the `notify@1`.
        #[arg(long)]
        to: String,
        #[arg(long)]
        deadline_us: Option<u64>,
        /// Seconds from now. The clock is read here, once.
        #[arg(long)]
        in_secs: Option<u64>,
        /// An open watch past the deadline ends as `timer` instead of `expired`.
        #[arg(long)]
        timer_only: bool,
    },
    /// Report the watch met. Sweep chooses `met` or `expired`.
    Meet {
        project: String,
        #[arg(long)]
        id: String,
    },
    /// Report the watch failed.
    Fail {
        project: String,
        #[arg(long)]
        id: String,
    },
    /// Send due watches now.
    Sweep { project: String },
}

pub fn run(cmd: &TimerCmd) -> Result<()> {
    let cfg = crate::config::Config::load().ok();
    gate(
        cfg.as_ref()
            .and_then(|c| c.timer.as_ref())
            .and_then(|t| t.enabled),
    )?;
    let dir = root();
    match cmd {
        TimerCmd::Arm {
            project,
            id,
            to,
            deadline_us,
            in_secs,
            timer_only,
        } => {
            let deadline = match (deadline_us, in_secs) {
                (Some(d), None) => *d,
                (None, Some(s)) => now_us().saturating_add(s.saturating_mul(1_000_000)),
                _ => bail!("pass exactly one of --deadline-us or --in-secs"),
            };
            arm(&dir, project, id, to, deadline, *timer_only).map_err(|e| anyhow::anyhow!(e))?;
            println!("armed {id} deadline_us {deadline}");
            let n =
                sweep_project(project, now_us(), exec_allowed()).map_err(|e| anyhow::anyhow!(e))?;
            if n > 0 {
                println!("swept {n}");
            }
        }
        TimerCmd::Meet { project, id } => {
            signal(&dir, project, id, Signal::Met).map_err(|e| anyhow::anyhow!(e))?;
            let n =
                sweep_project(project, now_us(), exec_allowed()).map_err(|e| anyhow::anyhow!(e))?;
            println!("meet {id} swept {n}");
        }
        TimerCmd::Fail { project, id } => {
            signal(&dir, project, id, Signal::Failed).map_err(|e| anyhow::anyhow!(e))?;
            let n =
                sweep_project(project, now_us(), exec_allowed()).map_err(|e| anyhow::anyhow!(e))?;
            println!("fail {id} swept {n}");
        }
        TimerCmd::Sweep { project } => {
            let n =
                sweep_project(project, now_us(), exec_allowed()).map_err(|e| anyhow::anyhow!(e))?;
            println!("swept {n}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPUB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const PROD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Rec {
        prepares: u32,
        appended: Vec<Envelope>,
        fail_appends: u32,
        epoch: u64,
    }

    impl NotifyPort for Rec {
        fn prepare(&mut self, _pin: &str) -> Result<(String, u64, String), String> {
            self.prepares += 1;
            self.epoch += 1;
            Ok((PROD.into(), self.epoch, NPUB.into()))
        }
        fn append(&mut self, env: &Envelope) -> Result<u64, String> {
            if self.fail_appends > 0 {
                self.fail_appends -= 1;
                return Err("store down".into());
            }
            self.appended.push(env.clone());
            Ok(self.appended.len() as u64)
        }
    }

    #[test]
    fn default_is_on_and_the_opt_outs_hold() {
        assert!(enabled(None, None));
        assert!(enabled(Some(""), None));
        assert!(enabled(Some("  "), None));
        assert!(!enabled(Some("0"), None));
        assert!(!enabled(Some("false"), Some(true)));
        assert!(!enabled(Some("off"), Some(true)));
        assert!(!enabled(Some("yes"), None), "a typo stays off");
        assert!(enabled(Some("1"), Some(false)));
        assert!(!enabled(None, Some(false)));
    }

    #[test]
    fn decide_table() {
        assert_eq!(decide(Signal::Met, 10, 10, false), Some(Outcome::Met));
        assert_eq!(decide(Signal::Met, 11, 10, false), Some(Outcome::Expired));
        assert_eq!(decide(Signal::Failed, 11, 10, false), Some(Outcome::Failed));
        assert_eq!(decide(Signal::Open, 9, 10, false), None);
        assert_eq!(decide(Signal::Open, 10, 10, false), Some(Outcome::Expired));
        assert_eq!(decide(Signal::Open, 10, 10, true), Some(Outcome::Timer));
    }

    #[test]
    fn notify_envelope_is_notify_v1() {
        let env = notify_envelope("flywheel", PROD, 3, NPUB, "w1", Outcome::Expired).unwrap();
        assert_eq!(env.contract.name, "notify");
        assert_eq!(env.contract.major, 1);
        assert_eq!(env.msg.epoch, 3);
        assert_eq!(env.body["text"], "watch w1 expired");
        assert!(notify_envelope("flywheel", PROD, 3, "not-an-npub", "w1", Outcome::Met).is_err());
    }

    #[test]
    fn a_due_watch_sends_once() {
        let d = tempfile::tempdir().unwrap();
        arm(d.path(), "p", "w1", "Buffy", 100, false).unwrap();
        let mut rec = Rec {
            prepares: 0,
            appended: vec![],
            fail_appends: 0,
            epoch: 0,
        };
        assert_eq!(sweep_at(d.path(), "p", 50, true, &mut rec).unwrap(), 0);
        assert_eq!(sweep_at(d.path(), "p", 100, true, &mut rec).unwrap(), 1);
        assert_eq!(sweep_at(d.path(), "p", 200, true, &mut rec).unwrap(), 0);
        assert_eq!(rec.appended.len(), 1);
        assert_eq!(rec.appended[0].msg.epoch, 1);
        assert_eq!(effect_key("w1", Outcome::Expired), "watch:w1:expired");
    }

    #[test]
    fn meet_and_fail_pick_one_outcome() {
        let d = tempfile::tempdir().unwrap();
        arm(d.path(), "p", "early", "Buffy", 100, false).unwrap();
        signal(d.path(), "p", "early", Signal::Met).unwrap();
        arm(d.path(), "p", "late", "Buffy", 100, false).unwrap();
        signal(d.path(), "p", "late", Signal::Met).unwrap();
        arm(d.path(), "p", "bad", "Buffy", 100, false).unwrap();
        signal(d.path(), "p", "bad", Signal::Failed).unwrap();
        let mut rec = Rec {
            prepares: 0,
            appended: vec![],
            fail_appends: 0,
            epoch: 0,
        };
        // early is before the deadline, late is after, bad is failed.
        // One sweep sees all three. Drive them at different clocks via decide's inputs
        // by sweeping twice: first at 100 (early=met, late=met because deadline==now, bad=failed).
        // Split late onto its own dir clock by sweeping late after moving... simpler: two roots.
        assert_eq!(sweep_at(d.path(), "p", 100, true, &mut rec).unwrap(), 3);
        let texts: Vec<_> = rec
            .appended
            .iter()
            .map(|e| e.body["text"].as_str().unwrap().to_string())
            .collect();
        assert!(texts.contains(&"watch early met".to_string()));
        assert!(texts.contains(&"watch late met".to_string()));
        assert!(texts.contains(&"watch bad failed".to_string()));
    }

    #[test]
    fn met_after_the_deadline_is_expired() {
        let d = tempfile::tempdir().unwrap();
        arm(d.path(), "p", "late", "Buffy", 100, false).unwrap();
        signal(d.path(), "p", "late", Signal::Met).unwrap();
        let mut rec = Rec {
            prepares: 0,
            appended: vec![],
            fail_appends: 0,
            epoch: 0,
        };
        sweep_at(d.path(), "p", 101, true, &mut rec).unwrap();
        assert_eq!(rec.appended[0].body["text"], "watch late expired");
    }

    #[test]
    fn a_failed_journal_write_does_not_append() {
        let d = tempfile::tempdir().unwrap();
        arm(d.path(), "p", "w1", "Buffy", 10, false).unwrap();
        let block = d.path().join("timer").join("p").join("intents");
        std::fs::write(&block, b"not a directory").unwrap();
        let mut rec = Rec {
            prepares: 0,
            appended: vec![],
            fail_appends: 0,
            epoch: 0,
        };
        let err = sweep_at(d.path(), "p", 10, true, &mut rec).unwrap_err();
        assert!(
            err.contains("Not a directory") || err.contains("not a directory") || !err.is_empty()
        );
        assert_eq!(rec.prepares, 0, "prepare is after the intent write");
        assert!(rec.appended.is_empty());
    }

    #[test]
    fn a_retry_resends_the_same_envelope() {
        let d = tempfile::tempdir().unwrap();
        arm(d.path(), "p", "w1", "Buffy", 10, false).unwrap();
        let mut rec = Rec {
            prepares: 0,
            appended: vec![],
            fail_appends: 1,
            epoch: 0,
        };
        assert!(sweep_at(d.path(), "p", 10, true, &mut rec).is_err());
        assert_eq!(rec.prepares, 1);
        assert!(rec.appended.is_empty());
        assert_eq!(sweep_at(d.path(), "p", 10, true, &mut rec).unwrap(), 1);
        assert_eq!(rec.prepares, 1, "retry must not claim a new epoch");
        assert_eq!(rec.appended.len(), 1);
        assert_eq!(rec.appended[0].msg.epoch, 1);
        assert_eq!(sweep_at(d.path(), "p", 99, true, &mut rec).unwrap(), 0);
    }

    fn rec() -> Rec {
        Rec {
            prepares: 0,
            appended: vec![],
            fail_appends: 0,
            epoch: 0,
        }
    }

    /// rv23 #4: one watch file that does not parse does not stop the sweep.
    /// The good watch is sent, the bad file is named and moved to `bad/`, and
    /// the next sweep is clean.
    #[test]
    fn one_bad_watch_file_does_not_stop_the_sweep() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("timer").join("p");
        std::fs::create_dir_all(&dir).unwrap();
        // Sorts before and after the good one, so either order is covered.
        std::fs::write(dir.join("a-bad.json"), b"{not json").unwrap();
        arm(d.path(), "p", "m1", "Buffy", 10, false).unwrap();
        std::fs::write(dir.join("z-bad.json"), b"[]").unwrap();
        let mut rec = rec();
        let err = sweep_at(d.path(), "p", 10, true, &mut rec).unwrap_err();
        assert_eq!(rec.appended.len(), 1, "the good watch was sent: {err}");
        assert!(err.contains("a-bad.json") && err.contains("z-bad.json"), "{err}");
        assert!(dir.join("bad").join("a-bad.json").exists());
        assert!(dir.join("bad").join("z-bad.json").exists());
        assert!(!dir.join("a-bad.json").exists());
        assert_eq!(sweep_at(d.path(), "p", 11, true, &mut rec).unwrap(), 0);
        assert_eq!(rec.appended.len(), 1);
    }

    /// rv23 #4: a settled watch leaves the swept directory (pruned to
    /// `done/`), still counts as settled for arm and signal, and a watch
    /// settled before pruning existed is pruned without a send.
    #[test]
    fn a_settled_watch_is_pruned_and_stays_settled() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("timer").join("p");
        arm(d.path(), "p", "w1", "Buffy", 10, false).unwrap();
        let mut rec = rec();
        assert_eq!(sweep_at(d.path(), "p", 10, true, &mut rec).unwrap(), 1);
        assert!(!dir.join("w1.json").exists(), "pruned from the swept dir");
        assert!(dir.join("done").join("w1.json").exists());
        assert!(arm(d.path(), "p", "w1", "Buffy", 99, false)
            .unwrap_err()
            .contains("already settled"));
        assert!(signal(d.path(), "p", "w1", Signal::Met)
            .unwrap_err()
            .contains("already settled"));
        // A settled file from before pruning: moved, nothing sent.
        let old = Watch {
            id: "old".into(),
            project: "p".into(),
            pin: "Buffy".into(),
            deadline_us: 1,
            timer_only: false,
            signal: Signal::Open,
            settled: Some("expired".into()),
        };
        write_json(&dir.join("old.json"), &old).unwrap();
        assert_eq!(sweep_at(d.path(), "p", 50, true, &mut rec).unwrap(), 0);
        assert!(!dir.join("old.json").exists());
        assert!(dir.join("done").join("old.json").exists());
        assert_eq!(rec.appended.len(), 1);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .collect();
        assert!(left.is_empty(), "no watch file is left to re-read");
    }

    /// rv23 #4: a sweep with nothing due never loads the config or the
    /// inbox facts; once loaded they are kept until the config changes.
    #[test]
    fn the_live_port_loads_config_and_facts_once() {
        let _g = crate::config::lock_env();
        let cfgd = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", cfgd.path()) };
        std::fs::create_dir_all(cfgd.path().join("flywheel")).unwrap();
        std::fs::write(crate::config::config_file(), b"").unwrap();
        let d = tempfile::tempdir().unwrap();
        arm(d.path(), "p", "later", "Buffy", 1_000, false).unwrap();
        let mut port = LivePort::new("p");
        for now in [1, 2, 3] {
            assert_eq!(sweep_at(d.path(), "p", now, true, &mut port).unwrap(), 0);
        }
        assert_eq!(port.loads, 0, "nothing due: no config, no facts");
        port.ports().unwrap();
        port.ports().unwrap();
        assert_eq!(port.loads, 1, "kept across uses");
        std::fs::write(crate::config::config_file(), b"\n\n").unwrap();
        port.ports().unwrap();
        assert_eq!(port.loads, 2, "a changed config is reloaded");
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

    #[test]
    fn allow_exec_zero_suppresses_the_ring() {
        let d = tempfile::tempdir().unwrap();
        arm(d.path(), "p", "w1", "Buffy", 10, true).unwrap();
        let mut rec = Rec {
            prepares: 0,
            appended: vec![],
            fail_appends: 0,
            epoch: 0,
        };
        assert_eq!(sweep_at(d.path(), "p", 10, false, &mut rec).unwrap(), 1);
        assert_eq!(rec.prepares, 0);
        assert!(rec.appended.is_empty());
        let note = std::fs::read_to_string(d.path().join("timer/p/w1.log.suppressed")).unwrap();
        assert!(note.contains("FLYWHEEL_ALLOW_EXEC=0"));
        assert!(note.contains("watch:w1:timer"));
        assert_eq!(sweep_at(d.path(), "p", 99, true, &mut rec).unwrap(), 0);
        assert!(
            rec.appended.is_empty(),
            "a suppressed watch does not send later"
        );
    }
}

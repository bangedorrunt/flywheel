// governed-by: ADR-0041
//! The flywheel side of the Phase 3 inbox host: [`FlywheelPorts`] implements
//! [`crate::inbox_host::Ports`] over the live machinery. This is the only
//! place the wake path touches the store, herdr and the filesystem.
//!
//! - observe: the spawn registry names the recipients; the in-process comms
//!   store answers `head` and `cursor`; herdr's `pane.list` gives the pane
//!   state and the `report_metadata` tokens the binding is verified against.
//! - ring: the closed [`crate::pane_write::PaneWrite::WakeLine`], by pane id.
//!   Nothing else can reach a pane from here.
//! - facts: `bind` and `deaf` facts append to
//!   `<state>/facts/<project-key>-inbox.jsonl`, keyed and idempotent.
//! - notify: a `notify@1` envelope appended to the supervisor's / captain's
//!   inbox under a synthetic, stable system producer (the doorbell has no
//!   key of its own); the normal wake then rings that pane like any other.
//! - urgency: the head envelope. A `steer@1` body whose `priority` is
//!   `urgent` is an urgent wake (interrupt, then the urgent line). Anything
//!   else, including a `notify@1` whose text says "urgent", is normal.
//!   The supervisor does not type that wake; appending is its job.

use crate::config::{self, Config};
use crate::herdr_shim::{AgentStatus, Client};
use crate::inbox_host::{Bind, Observation, Ports, Seen};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use toron_core::backend::herdr::{TOKEN_NPUB, TOKEN_PIN};
use toron_core::envelope::{Envelope, Recipient};
use toron_core::inbox::InboxStore;
use toron_core::inbox_program::DeafCause;
use toron_core::wake::{PaneState, Urgency};

/// How a verified binding is named in the `bind` fact.
pub const VERIFIED_BY: &str = "report_metadata";

pub struct FlywheelPorts {
    cfg: Config,
    project: String,
    workspace: String,
    store: Option<InboxStore>,
    npubs: BTreeMap<String, String>,
    herdr_was_down: bool,
    facts_seen: BTreeSet<String>,
    warned_store: bool,
    /// Head urgency from the last `observe`, keyed by npub. `ring` reads it.
    urgencies: BTreeMap<String, Urgency>,
}

/// Pure: herdr's word to the wake rule's pane state.
pub fn pane_state(s: AgentStatus) -> PaneState {
    match s {
        AgentStatus::Idle => PaneState::Idle,
        AgentStatus::Done => PaneState::Done,
        AgentStatus::Working => PaneState::Working,
        AgentStatus::Blocked => PaneState::Blocked,
        AgentStatus::Unknown => PaneState::Unknown,
    }
}

/// Pure: the binding for `pin`/`npub` from a pane's stamped tokens. Verified
/// only when BOTH the pin and the npub the spawn stamped match; a missing or
/// different stamp is an unverified binding (escalated, never rung).
pub fn verify_tokens(
    tokens: &serde_json::Map<String, Value>,
    pin: &str,
    npub: &str,
    pane_id: &str,
    session: &str,
) -> Bind {
    let get = |k: &str| tokens.get(k).and_then(Value::as_str).unwrap_or("");
    Bind {
        pane_id: pane_id.to_string(),
        herdr_session: session.to_string(),
        verified: get(TOKEN_PIN) == pin && get(TOKEN_NPUB) == npub && !npub.is_empty(),
        verified_by: VERIFIED_BY.to_string(),
    }
}

/// Pure: the system producer the doorbell's notifications are sent as (a
/// stable 64-hex id per project; no key, `from` only has to equal it).
pub fn system_producer(project: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(
        format!("flywheel-doorbell:{project}").as_bytes(),
    ))
}

/// Pure: the notice text for a deaf recipient.
pub fn deaf_text(pin: &str, wm: u64, cause: DeafCause) -> String {
    let why = match cause {
        DeafCause::NoAnswer => "was rung the whole budget and never ran `toron next`",
        DeafCause::Unbound => "has mail but no verified pane binding",
    };
    format!(
        "⟦toron⟧ DEAF: {} {why} (wm {wm:x}). Check its pane; mail stays in its inbox.",
        crate::redact::present(pin)
    )
}

impl FlywheelPorts {
    pub fn new(cfg: &Config, project: &str, workspace: &str) -> Self {
        let mut p = Self {
            cfg: cfg.clone(),
            project: project.to_string(),
            workspace: workspace.to_string(),
            store: None,
            npubs: BTreeMap::new(),
            herdr_was_down: false,
            facts_seen: BTreeSet::new(),
            warned_store: false,
            urgencies: BTreeMap::new(),
        };
        p.load_facts();
        p
    }

    fn facts_path(&self) -> std::path::PathBuf {
        config::state_dir().join("facts").join(format!(
            "{}-inbox.jsonl",
            crate::state::state_key(&self.project)
        ))
    }

    fn budget_path(&self) -> std::path::PathBuf {
        config::state_dir().join("inbox").join(format!(
            "{}-{}-ring-budget.json",
            crate::state::state_key(&self.project),
            crate::state::state_key(&self.workspace)
        ))
    }

    fn load_facts(&mut self) {
        if let Ok(s) = std::fs::read_to_string(self.facts_path()) {
            for l in s.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(l)
                    && let Some(k) = v.get("key").and_then(Value::as_str)
                {
                    self.facts_seen.insert(k.to_string());
                }
            }
        }
    }

    pub(crate) fn store(&mut self) -> Option<&InboxStore> {
        if self.store.is_none() {
            match crate::comms_send::store() {
                Ok(s) => self.store = Some(s),
                Err(e) => {
                    if !self.warned_store {
                        self.warned_store = true;
                        eprintln!("flywheel inbox: no in-process comms store ({e:#}); not ringing");
                    }
                }
            }
        }
        self.store.as_ref()
    }

    pub(crate) fn npub_of(&mut self, pin: &str) -> Option<String> {
        if let Some(n) = self.npubs.get(pin) {
            return Some(n.clone());
        }
        let key = crate::project_dir_str(&self.cfg, &self.project).ok()?;
        let mail = crate::backend::mail_backend(&self.cfg).ok()?;
        let w = crate::backend::run_mail(mail.whois(&key, pin)).ok()?;
        if w.npub.is_empty() {
            return None;
        }
        self.npubs.insert(pin.to_string(), w.npub.clone());
        Some(w.npub)
    }

    fn client(&self) -> Option<Client> {
        Client::connect(Some(&self.cfg.socket_path())).ok()
    }

    fn pin_of_npub(&self, npub: &str) -> String {
        self.npubs
            .iter()
            .find(|(_, n)| n.as_str() == npub)
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| npub.chars().take(8).collect())
    }

    /// The pin to notify for a role: a bound role, else the captain alias.
    fn role_pin(&self, roles: &[&str]) -> Option<String> {
        let binds = crate::state::load_role_binds(&self.project).unwrap_or_default();
        for r in roles {
            if let Some(p) = binds.get(*r) {
                return Some(p.clone());
            }
        }
        crate::state::load_captain_alias(&self.project)
    }

    fn notify_pin(&mut self, pin: &str, text: String, key: &str) -> Result<(), String> {
        self.append_notify(pin, text, key, 0).map(|_| ())
    }

    /// Append one `escalate@1` to `pin` as the system producer: `reason` is
    /// the notice, `key` the evidence that names it.
    fn escalate_pin(&mut self, pin: &str, reason: &str, key: &str) -> Result<(), String> {
        let to = self
            .npub_of(pin)
            .ok_or_else(|| format!("no npub for {pin}"))?;
        let (producer, epoch) = self.claim_producer_epoch()?;
        let env = crate::supervisor_host::escalate_envelope(
            &self.project,
            &producer,
            epoch,
            &to,
            reason,
            &[key.to_string()],
        )?;
        self.append_env(env).map(|_| ())
    }

    /// Append one `notify@1` to `pin`'s inbox as the system producer; returns
    /// the log offset. `generation` is the envelope's (a steer carries the
    /// child's incarnation).
    pub(crate) fn append_notify(
        &mut self,
        pin: &str,
        text: String,
        key: &str,
        generation: u64,
    ) -> Result<u64, String> {
        let to = self
            .npub_of(pin)
            .ok_or_else(|| format!("no npub for {pin}"))?;
        let producer = system_producer(&self.project);
        let store = self.store().ok_or("no comms store")?.clone();
        let (project, key) = (self.project.clone(), key.to_string());
        crate::mail_shim::block_on(async move {
            use toron_core::contracts::notify_v1::NotifyV1;
            use toron_core::envelope::{
                Class, ContextId, ContractId, ENVELOPE_V, Envelope, MsgId, Recipient,
            };
            let claim = store
                .claim_epoch(&producer, &format!("doorbell:{}", std::process::id()))
                .await
                .map_err(|e| e.to_string())?;
            let env = Envelope {
                v: ENVELOPE_V,
                msg: MsgId {
                    producer: producer.clone(),
                    epoch: claim.epoch,
                    seq: 0,
                },
                from: producer,
                to: Recipient::Npub(to),
                class: Class::Notify,
                contract: ContractId::new("notify", 1),
                context: ContextId(format!("{project}:{key}")),
                task: None,
                generation: u32::try_from(generation).unwrap_or(u32::MAX),
                causation: None,
                deadline_ts_us: None,
                lease_ms: None,
                body: serde_json::to_value(NotifyV1 { text }).map_err(|e| e.to_string())?,
            };
            env.validate().map_err(|e| format!("{e:?}"))?;
            store
                .append_envelope(&env)
                .await
                .map(|a| a.offset)
                .map_err(|e| e.to_string())
        })
    }

    /// Claim the next producer epoch for the doorbell's system producer.
    /// Callers that append `steer@1` / `escalate@1` use it so the triple is
    /// fresh. Flag-off never reaches here: only the supervisor host calls it,
    /// and that host runs only while the comms flag is on.
    pub(crate) fn claim_producer_epoch(&mut self) -> Result<(String, u64), String> {
        let producer = system_producer(&self.project);
        let epoch = self.claim_epoch_as(&producer)?;
        Ok((producer, epoch))
    }

    /// Claim the next epoch for `producer` (a nested lead steers as its own
    /// lead context's key, rv23 #6).
    pub(crate) fn claim_epoch_as(&mut self, producer: &str) -> Result<u64, String> {
        let store = self.store().ok_or("no comms store")?.clone();
        let p2 = producer.to_string();
        crate::mail_shim::block_on(async move {
            store
                .claim_epoch(&p2, &format!("doorbell:{}", std::process::id()))
                .await
                .map(|c| c.epoch)
                .map_err(|e| e.to_string())
        })
    }

    /// Append one already-validated envelope. The caller built it.
    pub(crate) fn append_env(
        &mut self,
        env: toron_core::envelope::Envelope,
    ) -> Result<u64, String> {
        let store = self.store().ok_or("no comms store")?.clone();
        crate::mail_shim::block_on(async move {
            env.validate().map_err(|e| format!("{e:?}"))?;
            store
                .append_envelope(&env)
                .await
                .map(|a| a.offset)
                .map_err(|e| e.to_string())
        })
    }
}

/// The two levels the fold reads for one recipient: the newest offset of its
/// mail (the watermark) and the cursor `toron next` commits.
///
/// The cursor is keyed `(reader, stream)` with the stream being the reader's
/// OWN npub (`toron next` defaults to it; naming anyone else's is refused).
/// `INBOX_STREAM` is the log's name, not a cursor stream: reading the cursor
/// there never sees an ack, so the host would ring an agent that had already
/// read (found by the live fixture; pinned by
/// `the_cursor_the_host_reads_is_the_one_toron_next_commits`).
pub(crate) async fn head_and_cursor(store: &InboxStore, npub: &str) -> (u64, u64) {
    let head = store.head(npub).await.unwrap_or(0);
    let cursor = store.cursor(npub, npub).await.unwrap_or(0);
    (head, cursor)
}

/// Urgency of one envelope. Only a `steer@1` body whose `priority` field is
/// the string `urgent` is urgent. A `normal` priority, any other value, and
/// every non-steer contract (including a `notify@1` whose text says "urgent")
/// are normal. This does not read `op` and does not parse message text.
pub(crate) fn urgency_of_envelope(env: &Envelope) -> Urgency {
    if env.contract.name != "steer" || env.contract.major != 1 {
        return Urgency::Normal;
    }
    match env.body.get("priority").and_then(Value::as_str) {
        Some("urgent") => Urgency::Urgent,
        _ => Urgency::Normal,
    }
}

/// The envelope stored at `offset` on the inbox stream, if it parses.
async fn envelope_at(store: &InboxStore, offset: u64) -> Option<Envelope> {
    let stream = toron_core::inbox::INBOX_STREAM;
    // `offset - 1` is the previous row when offsets are contiguous, so the
    // read is the head and anything newer, not the whole log. A gap (the
    // previous offset was never committed) falls back to a full read.
    let narrow = if offset > 1 {
        store.read_stream(stream, Some(offset - 1)).await.ok()
    } else {
        None
    };
    let rows = match narrow {
        Some(rows) if rows.iter().any(|(o, _)| *o == offset) => rows,
        _ => store.read_stream(stream, None).await.ok()?,
    };
    let payload = rows.into_iter().find(|(o, _)| *o == offset)?.1;
    serde_json::from_str(&payload).ok()
}

/// Urgency of `npub`'s head mail. `head == 0` is an empty inbox (the log's
/// first offset is 1). A head that is not this recipient's envelope, or that
/// does not parse, is normal: a missing read must not invent an urgent wake.
pub(crate) async fn urgency_of_head(store: &InboxStore, npub: &str, head: u64) -> Urgency {
    if head == 0 {
        return Urgency::Normal;
    }
    let Some(env) = envelope_at(store, head).await else {
        return Urgency::Normal;
    };
    match &env.to {
        Recipient::Npub(n) if n == npub => urgency_of_envelope(&env),
        _ => Urgency::Normal,
    }
}

/// What a ring did. `NoDoor` is the honest "nothing was typed": the harness
/// has no injection door (bd-harness-caps-6r1x), so the caller takes the
/// fallback instead of counting a wake that never happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RingDone {
    Typed,
    NoDoor(crate::harness_caps::NoDoor),
}

/// Type the wake for `wm`.
///
/// The fold always names the normal wake line; this checks that and then
/// types from `urgency`, which the caller took from the head envelope. An
/// urgent head is the urgent line. `route` decides whether there is a door at
/// all: [`crate::harness_caps::WakeRoute::Inject`] carries the Interrupt
/// choice (a hook-capable pane passes `interrupt = false`: the ring stays, the
/// next tool call is the hook's job), and a no-door route types nothing. A
/// normal head is the normal line only. One call is one wake.
pub(crate) fn ring_wake(
    typer: &dyn crate::pane_write::PaneTyper,
    pane_id: &str,
    wm: u64,
    line: &str,
    urgency: Urgency,
    route: crate::harness_caps::WakeRoute,
) -> Result<RingDone, String> {
    use crate::harness_caps::WakeRoute;
    use crate::pane_write::{PaneWrite, Urgency as U, Wm};
    let interrupt = match route {
        WakeRoute::Inject { interrupt } => interrupt,
        WakeRoute::NoDoor(no_door) => return Ok(RingDone::NoDoor(no_door)),
    };
    let normal = crate::pane_write::wake_line(Wm(wm), U::Normal);
    // The fold names the line; the sink renders it. They must agree, or
    // something other than the wake line is about to reach a pane.
    if normal != line {
        return Err("fold line differs from the pane sink's wake line".into());
    }
    match urgency {
        Urgency::Urgent => {
            if interrupt {
                crate::pane_write::write(typer, pane_id, &PaneWrite::Interrupt)
                    .map_err(|e| e.to_string())?;
            }
            crate::pane_write::write(
                typer,
                pane_id,
                &PaneWrite::WakeLine {
                    wm: Wm(wm),
                    urgency: U::Urgent,
                },
            )
            .map_err(|e| e.to_string())?;
        }
        Urgency::Normal => {
            crate::pane_write::write(
                typer,
                pane_id,
                &PaneWrite::WakeLine {
                    wm: Wm(wm),
                    urgency: U::Normal,
                },
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(RingDone::Typed)
}

impl Ports for FlywheelPorts {
    fn observe(&mut self) -> Observation {
        self.urgencies.clear();
        let registry = crate::state::load_spawn_registry(&self.project);
        let client = self.client();
        let herdr_restarted = client.is_some() && self.herdr_was_down;
        self.herdr_was_down = client.is_none();
        let panes = client
            .as_ref()
            .and_then(|c| c.pane_list(None).ok())
            .unwrap_or_default();
        let mut seen = Vec::new();
        for e in &registry {
            let Some(pin) = e.mail_identity.clone() else {
                continue;
            };
            let Some(npub) = self.npub_of(&pin) else {
                continue;
            };
            let Some(store) = self.store() else {
                self.urgencies.clear();
                return Observation::default();
            };
            let store = store.clone();
            let n2 = npub.clone();
            let (head, cursor, urgency) = crate::mail_shim::block_on(async move {
                let (head, cursor) = head_and_cursor(&store, &n2).await;
                let urgency = urgency_of_head(&store, &n2, head).await;
                (head, cursor, urgency)
            });
            self.urgencies.insert(npub.clone(), urgency);
            let pane = panes.iter().find(|p| p.pane_id == e.pane_id);
            let (state, bind) = match pane {
                Some(p) => (
                    pane_state(p.agent_status),
                    Some(verify_tokens(
                        &p.tokens,
                        &pin,
                        &npub,
                        &p.pane_id,
                        &p.workspace_id,
                    )),
                ),
                None if client.is_some() => (PaneState::Gone, None),
                None => (PaneState::Unknown, None),
            };
            seen.push(Seen {
                npub,
                pin,
                head,
                cursor,
                urgency,
                pane: state,
                bind,
            });
        }
        Observation {
            seen,
            herdr_restarted,
        }
    }

    fn reverify(&mut self, npub: &str) -> Option<Bind> {
        let pin = self.pin_of_npub(npub);
        let registry = crate::state::load_spawn_registry(&self.project);
        let e = registry
            .iter()
            .find(|e| e.mail_identity.as_deref() == Some(pin.as_str()))?;
        let client = self.client()?;
        let p = client
            .pane_list(None)
            .ok()?
            .into_iter()
            .find(|p| p.pane_id == e.pane_id)?;
        Some(verify_tokens(
            &p.tokens,
            &pin,
            npub,
            &p.pane_id,
            &p.workspace_id,
        ))
    }

    fn ring(&mut self, npub: &str, pane_id: &str, wm: u64, line: &str) -> Result<(), String> {
        let client = self.client().ok_or("herdr unreachable")?;
        let urgency = self.urgencies.get(npub).copied().unwrap_or(Urgency::Normal);
        let kind = crate::state::load_spawn_registry(&self.project)
            .into_iter()
            .find(|e| e.pane_id == pane_id)
            .and_then(|e| e.harness)
            .unwrap_or_default();
        // The harness record decides whether there is a door at all. An
        // unknown kind has none: the ring fails loudly naming mail rather
        // than typing at a pane that cannot be poked.
        let route =
            crate::harness_caps::wake_route(&kind, crate::harness_caps::installed_from_env());
        match ring_wake(
            &crate::pane_write::HerdrPanes(&client),
            pane_id,
            wm,
            line,
            urgency,
            route,
        )? {
            RingDone::Typed => Ok(()),
            RingDone::NoDoor(no_door) => Err(crate::harness_caps::no_door_line(&no_door)),
        }
    }

    fn load_budget(&mut self) -> BTreeMap<String, crate::inbox_host::Budget> {
        std::fs::read_to_string(self.budget_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_budget(
        &mut self,
        budget: &BTreeMap<String, crate::inbox_host::Budget>,
    ) -> Result<(), String> {
        let path = self.budget_path();
        if let Some(dir) = path.parent() {
            crate::private_fs::create_private_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // Write-then-rename: a crash leaves the old file or the new, never half.
        crate::private_fs::write_private_atomic(
            &path,
            &serde_json::to_vec(budget).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }

    fn fact(&mut self, key: &str, mut fact: Value) -> Result<(), String> {
        if self.facts_seen.contains(key) {
            return Ok(());
        }
        if let Some(o) = fact.as_object_mut() {
            o.insert("key".into(), json!(key));
            o.insert("workspace".into(), json!(self.workspace));
            o.insert("ts".into(), json!(chrono::Utc::now().to_rfc3339()));
        }
        let path = self.facts_path();
        if let Some(dir) = path.parent() {
            crate::private_fs::create_private_dir_all(dir).map_err(|e| e.to_string())?;
        }
        use std::io::Write as _;
        let mut f = crate::private_fs::open_private_append(&path).map_err(|e| e.to_string())?;
        writeln!(f, "{fact}").map_err(|e| e.to_string())?;
        self.facts_seen.insert(key.to_string());
        Ok(())
    }

    fn notify_supervisor(&mut self, npub: &str, wm: u64, cause: DeafCause) -> Result<(), String> {
        let key = format!("deaf-supervisor:{npub}:{wm}");
        if self.facts_seen.contains(&key) {
            return Ok(());
        }
        let who = self.pin_of_npub(npub);
        let Some(sup) = self.role_pin(&["supervisor", "lead"]) else {
            return Err("no supervisor, lead or captain pin bound".into());
        };
        self.notify_pin(&sup, deaf_text(&who, wm, cause), &key)?;
        self.fact(
            &key,
            json!({"kind": "notified", "to": sup, "role": "supervisor"}),
        )
    }

    fn notify_captain(&mut self, npub: &str, wm: u64, cause: DeafCause) -> Result<(), String> {
        let key = format!("deaf-captain:{npub}:{wm}");
        if self.facts_seen.contains(&key) {
            return Ok(());
        }
        let who = self.pin_of_npub(npub);
        let text = deaf_text(&who, wm, cause);
        // The loud, unconditional leg: the captain surface herdr offers us
        // is a pane, and that is the inbox wake; this line is for the log.
        eprintln!("flywheel inbox: CAPTAIN: {text}");
        let Some(cap) = crate::state::load_captain_alias(&self.project) else {
            return self.fact(
                &key,
                json!({"kind": "notified", "to": null, "role": "captain"}),
            );
        };
        // rv23 #6: a deaf recipient is a captain decision, so the notice is
        // `escalate@1` (Control), not a `notify@1` the captain may never act
        // on. The supervisor's deaf notice stays `notify@1`.
        self.escalate_pin(&cap, &text, &key)?;
        self.fact(
            &key,
            json!({"kind": "notified", "to": cap, "role": "captain"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_core::envelope::{Class, ContextId, ContractId, ENVELOPE_V, MsgId, TaskId};
    use toron_core::inbox::InboxStore;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_deaf_notice_quotes_the_pin_and_hides_a_token() {
        let token = "ghp_ABCDEFGHIJKLMNOPQRST";
        let pin = format!("TealBirch\n{token}");
        let text = deaf_text(&pin, 0xab, DeafCause::NoAnswer);
        assert!(!text.contains('\n'), "{text}");
        assert!(!text.contains(token), "{text}");
        assert!(text.contains("`TealBirch"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
    }

    fn env(name: &str, major: u32, class: Class, body: Value, task: bool) -> Envelope {
        Envelope {
            v: ENVELOPE_V,
            msg: MsgId {
                producer: A.into(),
                epoch: 0,
                seq: 0,
            },
            from: A.into(),
            to: Recipient::Npub(B.into()),
            class,
            contract: ContractId::new(name, major),
            context: ContextId("proj".into()),
            task: task.then(|| TaskId("crew".into())),
            generation: 1,
            causation: None,
            deadline_ts_us: None,
            lease_ms: None,
            body,
        }
    }

    fn steer(op: &str, priority: &str) -> Envelope {
        env(
            "steer",
            1,
            Class::Control,
            json!({"op": op, "priority": priority, "sup_epoch": 0}),
            true,
        )
    }

    /// A pane that records every write.
    #[derive(Default)]
    struct Rec(std::sync::Mutex<Vec<String>>);

    impl crate::pane_write::PaneTyper for Rec {
        fn prompt(&self, target: &str, line: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("{target} line {line}"));
            Ok(())
        }
        fn keys(&self, target: &str, keys: &[&str]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{target} keys {}", keys.join("+")));
            Ok(())
        }
        fn close(&self, target: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("{target} close"));
            Ok(())
        }
    }

    fn fold_line(wm: u64) -> String {
        crate::pane_write::wake_line(
            crate::pane_write::Wm(wm),
            crate::pane_write::Urgency::Normal,
        )
    }

    #[test]
    fn steer_priority_makes_the_observation_urgent_and_a_non_steer_does_not() {
        let urgent = steer("resume", "urgent");
        let normal = steer("cancel", "normal");
        let notify = env(
            "notify",
            1,
            Class::Notify,
            json!({"text": "priority: urgent and op != resume"}),
            false,
        );
        // Priority is the field. `op` and the notice text are not.
        assert_eq!(urgency_of_envelope(&urgent), Urgency::Urgent);
        assert_eq!(urgency_of_envelope(&normal), Urgency::Normal);
        assert_eq!(urgency_of_envelope(&notify), Urgency::Normal);

        let pane = Rec::default();
        let interrupt = crate::harness_caps::WakeRoute::Inject { interrupt: true };
        ring_wake(
            &pane,
            "w1:a",
            7,
            &fold_line(7),
            urgency_of_envelope(&urgent),
            interrupt,
        )
        .unwrap();
        ring_wake(
            &pane,
            "w1:a",
            8,
            &fold_line(8),
            urgency_of_envelope(&notify),
            interrupt,
        )
        .unwrap();
        let writes = pane.0.lock().unwrap().clone();
        let urgent_lines = writes.iter().filter(|w| w.contains("URGENT steer")).count();
        assert_eq!(urgent_lines, 1, "{writes:?}");
        assert!(
            writes.iter().any(|w| w.contains("URGENT steer (wm 7)")),
            "{writes:?}"
        );
        assert_eq!(
            writes.iter().filter(|w| w.contains("keys")).count(),
            1,
            "one interrupt for the urgent observation: {writes:?}"
        );
        assert!(
            writes
                .iter()
                .any(|w| w.contains("new work") && w.contains('8')),
            "the non-steer observation is a normal wake: {writes:?}"
        );
        assert!(
            !writes
                .iter()
                .any(|w| w.contains("URGENT") && w.contains('8')),
            "a non-steer does not ring urgent: {writes:?}"
        );
    }

    async fn mem_store() -> InboxStore {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        let pool = toron_core::index::schema::connect(&db.to_string_lossy())
            .await
            .unwrap();
        toron_core::index::schema::migrate(&pool).await.unwrap();
        InboxStore::new(pool)
    }

    fn stored(seq: u64, class: Class, name: &str, body: Value, task: bool) -> Envelope {
        let mut e = env(name, 1, class, body, task);
        e.msg.seq = seq;
        e
    }

    /// rv23 #6: the deaf CAPTAIN notice is escalate@1; the supervisor's stays
    /// notify@1, and nothing else changed shape.
    #[test]
    fn the_deaf_captain_notice_is_an_escalate_and_the_supervisor_notice_a_notify() {
        let src = include_str!("inbox_ports.rs");
        let body = |name: &str| {
            let at = src.find(name).expect(name);
            &src[at..at + src[at..].find("\n    }\n").expect("end")]
        };
        let cap = body("fn notify_captain(");
        assert!(cap.contains("self.escalate_pin(&cap, &text, &key)?"), "{cap}");
        assert!(!cap.contains("notify_pin("), "{cap}");
        let sup = body("fn notify_supervisor(");
        assert!(sup.contains("self.notify_pin(&sup,"), "{sup}");
        let esc = body("fn escalate_pin(");
        assert!(esc.contains("escalate_envelope("), "{esc}");
        let env = crate::supervisor_host::escalate_envelope(
            "proj",
            A,
            0,
            B,
            &deaf_text("Pin", 0xab, DeafCause::NoAnswer),
            &["deaf-captain:x:171".into()],
        )
        .unwrap();
        assert_eq!(env.contract, ContractId::new("escalate", 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stored_steer_head_is_urgent_and_a_later_notify_head_is_not() {
        // rv23 #6 (toron F7): with no lead row only the operator may steer,
        // so the steering key `A` is the store's operator. Pre-F7 toron has
        // no such fence; this passes on both.
        let store = mem_store().await.with_operator(A.to_string());
        let steer = stored(
            0,
            Class::Control,
            "steer",
            json!({"op": "pause", "priority": "urgent", "sup_epoch": 0}),
            true,
        );
        steer.validate().unwrap();
        store.append_envelope(&steer).await.unwrap();
        let head = store.head(B).await.unwrap();
        assert_eq!(urgency_of_head(&store, B, head).await, Urgency::Urgent);
        let notify = stored(
            1,
            Class::Notify,
            "notify",
            json!({"text": "priority: urgent"}),
            false,
        );
        notify.validate().unwrap();
        store.append_envelope(&notify).await.unwrap();
        let head = store.head(B).await.unwrap();
        assert_eq!(urgency_of_head(&store, B, head).await, Urgency::Normal);
        let normal = stored(
            2,
            Class::Control,
            "steer",
            json!({"op": "resume", "priority": "normal", "sup_epoch": 0}),
            true,
        );
        normal.validate().unwrap();
        store.append_envelope(&normal).await.unwrap();
        let head = store.head(B).await.unwrap();
        assert_eq!(urgency_of_head(&store, B, head).await, Urgency::Normal);
    }
}

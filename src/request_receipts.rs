//! bd-command-receipts-zile: request-key receipts for mutating doors.
//!
//! A retried mutation has to converge, not repeat. The spawn door already
//! journals its intent before dispatch ([`crate::agent_ref::AgentEffectGuard`],
//! I4) and keys that row by `(project, subject, phase)` through
//! `derive_effect_key` — one row per logical slot, not an append-only pile of
//! duplicates. That journal IS the receipt: what a retry needs to know is
//! whether the slot it is about to dispatch was ever settled.
//!
//! The request key makes the slot stable: the workspace label derives from it,
//! so a retry lands on the same subject and therefore the same row, and
//! [`spawn_gate`] answers with that row's last state. Nothing here dispatches
//! and nothing here writes — the door commits its own row at the same boundary
//! it always did.

use crate::agent_ref::{AgentEffect, EffectState};

/// What a door does with a request key, given the last journaled row for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// No row, or a row whose dispatch never settled: dispatch. The row is
    /// written at the same boundary as before.
    Proceed,
    /// The row settled: the effect already happened. Replay the recorded
    /// slot; never dispatch a second time.
    Replay,
    /// The row is unresolved (`Prepared` / `InFlight`): a dispatch may be in
    /// flight right now, and a second one could fork a crew into one
    /// workspace. Refuse loudly rather than race it.
    InFlight,
    /// The key is already used for a DIFFERENT plan. A replay would hand back
    /// another crew's slot, so this is a refusal, never a silent ok.
    Conflict,
}

/// The pure gate core. One rule, two row shapes ([`spawn_gate`] over an agent
/// effect, [`receipt_gate`] over a door receipt): `offered_digest` is the
/// digest of what the caller is asking for; a row recorded under the same slot
/// with another digest is [`Gate::Conflict`].
///
/// `Unknown`/`Failed` rows proceed: both paths are at-least-once by documented
/// residual, the key's job is to make the retry land on the same slot, and the
/// door's own guards stay the last line.
fn gate_core(state: EffectState, recorded: Option<&str>, offered_digest: &str) -> Gate {
    match state {
        // Prepared, InFlight, Accepted, ReconcilePending and Parked are all
        // "the effect has not reached a terminal verdict" (bd-ozpj: the
        // adopted enum is toron-core's nine-state one, so the states that
        // flywheel's old five-state copy could not express arrive here).
        EffectState::Prepared
        | EffectState::InFlight
        | EffectState::Accepted
        | EffectState::ReconcilePending
        | EffectState::Parked => Gate::InFlight,
        EffectState::Settled => match recorded {
            Some(recorded) if recorded != offered_digest => Gate::Conflict,
            // A row from before digests were recorded cannot be compared; the
            // slot settled, so the replay is still the answer.
            _ => Gate::Replay,
        },
        // A cancelled effect did NOT happen: the slot is free for a new
        // attempt, exactly like a failed or unknown one.
        EffectState::Failed | EffectState::Unknown | EffectState::Cancelled => Gate::Proceed,
    }
}

/// The gate for one spawn/add request key.
pub fn spawn_gate(row: Option<&AgentEffect>, offered_digest: &str) -> Gate {
    match row {
        None => Gate::Proceed,
        Some(row) => gate_core(row.state, row.plan_digest.as_deref(), offered_digest),
    }
}

/// The label a request key pins. Deterministic and slug-shaped, so a retry
/// computes the same workspace label — and therefore the same effect row — as
/// the first attempt.
pub fn label_for_request_key(base: &str, key: &str) -> String {
    let joined = format!("{base}-rk-{key}");
    crate::slugify_label(&joined).0
}

/// A stable digest of the spawn plan a request key is used for. Sorted so the
/// same plan in another order is the same plan; the version tag keeps a later
/// plan shape from colliding with an older digest.
pub fn plan_digest(items: &[(String, u32, Option<String>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut rows: Vec<String> = items
        .iter()
        .map(|(kind, count, model)| {
            format!(
                "{}\u{1f}{}\u{1f}{}",
                kind.trim().to_ascii_lowercase(),
                count,
                model.as_deref().unwrap_or("-").trim().to_ascii_lowercase()
            )
        })
        .collect();
    rows.sort();
    let mut h = Sha256::new();
    h.update(b"gs043-spawn-plan-v1\0");
    h.update(rows.join("\n").as_bytes());
    format!("plan:{}", hex::encode(h.finalize()))
}

/// Delivery word for every send (ADR-0019 D5). Canonical type lives in
/// `captain_directory` (single constructor, no `From<&str>`); re-exported
/// here so receipt call sites keep one path. The log stores the word via
/// `settle`.
pub use crate::captain_directory::Delivery;

/// Typed steer payload over the digest inputs (ADR-0019 D10). The digest
/// is re-derived from these typed fields with `payload_digest` — never a
/// canonical-JSON rehash (serializer byte-stability is nobody's guarantee).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SteerPayload {
    pub objective: String,
    #[serde(default)]
    pub crew: String,
    #[serde(default)]
    pub direct: bool,
    #[serde(default)]
    pub reuse: String,
    #[serde(default)]
    pub retain: bool,
    #[serde(default)]
    pub retire_orphans: bool,
    #[serde(default)]
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
}

impl SteerPayload {
    pub fn digest(&self) -> String {
        payload_digest(
            "steer-v1",
            &[
                &self.objective,
                &self.crew,
                if self.direct { "direct" } else { "goal" },
                &self.reuse,
                if self.retain { "retain" } else { "reap" },
                if self.retire_orphans {
                    "orphans"
                } else {
                    "keep"
                },
            ],
        )
    }
}

/// File-hold notice window: one mail per (bead, holder) per 15 min (D9).
pub const FILE_HOLD_WINDOW_SECS: u64 = 900;

/// One request-key slot for a door that is NOT an agent lifecycle: a steer
/// dispatch or a halt. Same four answers as the agent shape (one rule, through
/// [`gate_core`]), but the row is the door's own, tagged `door_receipt` in the
/// log so a reader can tell it from an agent effect row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DoorReceipt {
    pub door: String,
    pub slot: String,
    pub state: EffectState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Typed steer payload (D10). Required while a hosted steer is `Parked`;
    /// old rows read as `None`. Approve never takes a new objective — the
    /// payload is the only replay source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<SteerPayload>,
    /// Asker pin recorded at park time; approve refuses when `--as` equals it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asker: Option<String>,
    /// Park timestamp in unix millis; approve expires past APPROVAL_WAIT_MS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_at_ms: Option<u64>,
}

impl DoorReceipt {
    pub fn prepare(door: &str, slot: &str, digest: &str) -> Self {
        Self {
            door: door.to_string(),
            slot: slot.to_string(),
            state: EffectState::Prepared,
            digest: Some(digest.to_string()),
            evidence: None,
            payload: None,
            asker: None,
            parked_at_ms: None,
        }
    }

    /// Evidence word recorded at settle time (compat reader for old rows).
    #[cfg_attr(not(test), allow(dead_code))] // compat reader for pre-payload rows; pinned by test
    pub fn evidence_word(&self) -> Option<&str> {
        self.evidence.as_deref()
    }
}

/// Intent-before-dispatch for a door: `Prepared` then `InFlight`, journaled
/// before the door performs anything. A dispatch whose intent cannot be
/// committed does not run — the caller gets the error and must not proceed.
pub struct DoorReceiptGuard {
    project: String,
    receipt: DoorReceipt,
}

impl DoorReceiptGuard {
    pub fn begin(project: &str, door: &str, slot: &str, digest: &str) -> anyhow::Result<Self> {
        let mut receipt = DoorReceipt::prepare(door, slot, digest);
        journal_receipt(project, &receipt)?;
        receipt.state = EffectState::InFlight;
        journal_receipt(project, &receipt)?;
        Ok(Self {
            project: project.to_string(),
            receipt,
        })
    }

    /// Apply delivery word observed (the dispatch really ran): `Settled`.
    /// The ONLY setter for `Settled` evidence — no `From<&str>` for
    /// `Delivery`, no direct `.evidence =` outside this module.
    pub fn settle(mut self, delivery: Delivery) {
        match delivery {
            Delivery::Unknown => {
                self.receipt.state = EffectState::Unknown;
            }
            _ => {
                self.receipt.state = EffectState::Settled;
            }
        }
        self.receipt.evidence = Some(delivery.as_str().to_string());
        let _ = journal_receipt(&self.project, &self.receipt);
    }

    /// The dispatch ran and its outcome is unobservable: `Unknown` — never a
    /// silent absence, never an invented `Settled`.
    pub fn mark_unknown(mut self, evidence: &str) {
        self.receipt.state = EffectState::Unknown;
        self.receipt.evidence = Some(evidence.to_string());
        let _ = journal_receipt(&self.project, &self.receipt);
    }

    /// The dispatch was refused before it could run (an aborted gate, a failed
    /// precondition): `Failed`. A later attempt under the same key proceeds.
    pub fn mark_failed(mut self, evidence: &str) {
        self.receipt.state = EffectState::Failed;
        self.receipt.evidence = Some(evidence.to_string());
        let _ = journal_receipt(&self.project, &self.receipt);
    }

    /// ADR-0019 D9: file-hold settle. Only after toron accepted the holder
    /// mail — the steer did not dispatch because the target bead's paths
    /// are held. `Settled` so a same-key retry replays instead of forking
    /// a second steer while the hold stands.
    pub fn settle_file_held(mut self) {
        self.receipt.state = EffectState::Settled;
        self.receipt.evidence = Some("file_held".to_string());
        let _ = journal_receipt(&self.project, &self.receipt);
    }
}

fn journal_receipt(project: &str, receipt: &DoorReceipt) -> anyhow::Result<()> {
    let mut entry = serde_json::to_value(receipt)?;
    // Tagged so a reader can tell a door row from an agent effect row.
    if let serde_json::Value::Object(map) = &mut entry {
        map.insert(
            "journal".to_string(),
            serde_json::Value::String("door_receipt".to_string()),
        );
    }
    crate::state::log_event(project, crate::state::EventKind::State, entry)?;
    eprintln!(
        "flywheel: door receipt {} slot={} state={:?}",
        receipt.door, receipt.slot, receipt.state
    );
    Ok(())
}

/// The slot a door's request key pins: `"<project>-<door>-rk-<key>"`, slugged.
/// Deterministic, so a retry reads the same row the first attempt wrote, and
/// namespaced per door so a steer key can never collide with a halt key.
pub fn door_slot(project: &str, door: &str, key: &str) -> String {
    label_for_request_key(&format!("{project}-{door}"), key)
}

/// A stable digest of a door's payload, version-tagged like [`plan_digest`] so
/// a later payload shape cannot collide with an older digest. Parts are
/// trimmed and joined with a separator that cannot appear in a trimmed field.
pub fn payload_digest(tag: &str, parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    h.update(b"\0");
    for p in parts {
        h.update(p.trim().as_bytes());
        h.update(b"\x1f");
    }
    format!("{tag}:{}", hex::encode(h.finalize()))
}

/// The gate for one door request key.
pub fn receipt_gate(row: Option<&DoorReceipt>, offered_digest: &str) -> Gate {
    match row {
        None => Gate::Proceed,
        Some(row) => gate_core(row.state, row.digest.as_deref(), offered_digest),
    }
}

/// The last journaled door row for `(door, slot)`. Missing log, unreadable
/// rows, and rows of another door all read `None` — an absent receipt is not a
/// settled one.
pub fn load_door_receipt(project: &str, door: &str, slot: &str) -> Option<DoorReceipt> {
    let log = crate::state::read_log(project).ok()?;
    log.into_iter().rev().find_map(|e| {
        let data = e.data;
        if data.get("journal").and_then(|v| v.as_str()) != Some("door_receipt") {
            return None;
        }
        let row: DoorReceipt = serde_json::from_value(data).ok()?;
        (row.door == door && row.slot == slot).then_some(row)
    })
}

/// Hosted caller: no rex block in this process (ADR-0019 D5). No block
/// means `REX_SESSION` empty/unset. A captain sitting in a rex block is
/// local and never takes the approval park.
pub fn is_hosted_caller() -> bool {
    std::env::var("REX_SESSION")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
}

/// Current unix millis for park timestamps (passed in as arg at pure call
/// sites; this is the live clock edge only).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write `Prepared` then `Parked` with the typed payload (ADR-0019 D10
/// hosted steer order). A failed journal insert returns the error and leaves
/// no parked row — the caller must not dispatch.
pub fn park_steer(
    project: &str,
    slot: &str,
    digest: &str,
    payload: SteerPayload,
    asker: &str,
) -> anyhow::Result<DoorReceipt> {
    let mut receipt = DoorReceipt::prepare("steer", slot, digest);
    receipt.payload = Some(payload);
    receipt.asker = Some(asker.to_string());
    receipt.parked_at_ms = Some(now_ms());
    // Prepared first so a crash between writes reads as in-flight, never
    // as settled.
    journal_receipt(project, &receipt)?;
    receipt.state = EffectState::Parked;
    journal_receipt(project, &receipt)?;
    Ok(receipt)
}

/// One parked steer per captain pin: any `Parked` steer receipt in this
/// project for the same asker with a different slot blocks a second hosted
/// steer. Only the latest row per slot counts — history rows (a slot that
/// later settled) do not hold the pin.
pub fn has_other_parked_steer(project: &str, asker: &str, except_slot: &str) -> bool {
    let Ok(log) = crate::state::read_log(project) else {
        return false;
    };
    let mut parsed: Vec<DoorReceipt> = Vec::new();
    for e in log.iter() {
        let data = &e.data;
        if data.get("journal").and_then(|v| v.as_str()) != Some("door_receipt") {
            continue;
        }
        let Ok(row): Result<DoorReceipt, _> = serde_json::from_value(data.clone()) else {
            continue;
        };
        parsed.push(row);
    }
    latest_parked_other(&parsed, asker, except_slot)
}

/// Pure core of [`has_other_parked_steer`]: only the latest row per slot
/// counts — history rows (a slot that later settled) do not hold the pin.
pub fn latest_parked_other(
    rows: &[DoorReceipt],
    asker: &str,
    except_slot: &str,
) -> bool {
    let mut latest: std::collections::BTreeMap<(String, String), &DoorReceipt> =
        std::collections::BTreeMap::new();
    for row in rows {
        latest.insert((row.door.clone(), row.slot.clone()), row);
    }
    latest.values().any(|row| {
        row.door == "steer"
            && row.state == EffectState::Parked
            && row.slot != except_slot
            && row.asker.as_deref() == Some(asker)
    })
}

/// The last journaled spawn row for `subject`, read from the project event
/// log. Missing log, unreadable rows, and a row for another phase all read
/// `None` — an absent receipt is not a settled one.
pub fn load_spawn_row(project: &str, subject: &str) -> Option<AgentEffect> {
    let log = crate::state::read_log(project).ok()?;
    log.into_iter().rev().find_map(|e| {
        let data = e.data;
        let tagged = data.get("journal").and_then(|v| v.as_str()) == Some("agent_effect");
        if !tagged {
            return None;
        }
        let row: AgentEffect = serde_json::from_value(data).ok()?;
        (row.subject == subject && row.phase == crate::agent_ref::AgentPhase::Spawn).then_some(row)
    })
}

/// Journal one door row directly (state transitions the guard does not own:
/// Parked→InFlight on approve, InFlight→Settled/Denied at the outcome). The
/// caller owns the transition's validity; the journal only records it.
pub fn journal_door_row(project: &str, receipt: &DoorReceipt) -> anyhow::Result<()> {
    journal_receipt(project, receipt)
}

/// Re-derive check: the parked payload still hashes to the row's digest.
/// A digest is not reversible, so the payload is the only replay source; a
/// mismatch is `conflict` and the row stays `Parked`.
pub fn payload_rederives(payload: &SteerPayload, digest: &str) -> bool {
    payload.digest() == digest
}

/// Approval mail body, exactly (ADR-0019 D10). Four lines; a fifth
/// `project=<slug>` line only for a cross-project hosted steer. The note is
/// one line, cut at 500 characters with the cut marked.
pub fn approval_body(
    request_key: &str,
    decision: &str,
    note: &str,
    project: Option<&str>,
) -> String {
    let one_line: String = note.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = one_line.chars().collect();
    let cut = if chars.len() > 500 {
        format!("{} [cut]", chars[..500].iter().collect::<String>())
    } else {
        one_line
    };
    let mut out =
        format!("flywheel-approval.v1\nabout={request_key}\ndecision={decision}\nnote={cut}");
    if let Some(p) = project {
        out.push_str(&format!("\nproject={p}"));
    }
    out
}

/// Parsed approval body. `None` = `approval_mismatch`: prose that says
/// "approved" without these lines never authorizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalBody {
    pub request_key: String,
    pub decision: String,
    pub note: String,
    pub project: Option<String>,
}

pub fn parse_approval_body(body: &str) -> Option<ApprovalBody> {
    let mut lines = body.lines();
    if lines.next()? != "flywheel-approval.v1" {
        return None;
    }
    let about = lines.next()?.strip_prefix("about=")?.to_string();
    let decision = lines.next()?.strip_prefix("decision=")?.to_string();
    if decision != "approve" && decision != "deny" {
        return None;
    }
    let note = lines.next()?.strip_prefix("note=")?.to_string();
    let project = match lines.next() {
        None => None,
        Some(l) => Some(l.strip_prefix("project=")?.to_string()),
    };
    if lines.next().is_some() {
        return None;
    }
    if about.is_empty() {
        return None;
    }
    Some(ApprovalBody {
        request_key: about,
        decision,
        note,
        project,
    })
}

/// Approval expiry (ADR-0019 D10): past the bound the slot settles `Unknown`
/// with evidence `approval_expired`. Pure over millis.
pub fn approval_expired(parked_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(parked_at_ms) > crate::captain_directory::APPROVAL_WAIT_MS as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_ref::{AgentEffect, AgentPhase};

    fn row(state: EffectState, digest: Option<&str>) -> AgentEffect {
        let mut r = AgentEffect::prepare("flywheel", "flywheel-flywheel-rk-abc", AgentPhase::Spawn);
        r.state = state;
        r.plan_digest = digest.map(str::to_string);
        r
    }

    /// bd-command-receipts-zile: one key, one dispatch. The second attempt
    /// with the same key and plan replays the recorded slot and performs no
    /// second effect; an unresolved slot refuses; a different plan under the
    /// same key is a typed conflict.
    #[test]
    fn request_key_replay_is_idempotent() {
        let plan = plan_digest(&[("opencode".into(), 3, None)]);
        assert_eq!(
            plan,
            plan_digest(&[("opencode".into(), 3, None)]),
            "the same plan digests the same"
        );
        assert_ne!(
            plan,
            plan_digest(&[("opencode".into(), 4, None)]),
            "another crew size is another plan"
        );
        assert_ne!(
            plan,
            plan_digest(&[("opencode".into(), 3, Some("pi/muse".into()))]),
            "another model is another plan"
        );

        // The label a key pins is stable and slug-shaped, so a retry reads the
        // same slot as the first attempt.
        let label = label_for_request_key("flywheel-flywheel", "Deploy 42");
        assert_eq!(
            label,
            label_for_request_key("flywheel-flywheel", "Deploy 42")
        );
        assert_eq!(label, "flywheel-flywheel-rk-deploy-42");
        assert_ne!(
            label,
            label_for_request_key("flywheel-flywheel", "Deploy 43")
        );

        // The door's contract, driven through the same gate it uses: the
        // second attempt must not touch the dispatch.
        let mut journal: std::collections::BTreeMap<String, AgentEffect> =
            std::collections::BTreeMap::new();
        let mut dispatched = 0u32;
        let attempt = |journal: &mut std::collections::BTreeMap<String, AgentEffect>,
                       dispatched: &mut u32|
         -> Gate {
            let key = "Deploy 42".to_string();
            let gate = spawn_gate(journal.get(&key), &plan);
            if gate == Gate::Proceed {
                *dispatched += 1;
                let mut settled = row(EffectState::Settled, Some(&plan));
                settled.state = EffectState::Settled;
                journal.insert(key, settled);
            }
            gate
        };
        assert_eq!(attempt(&mut journal, &mut dispatched), Gate::Proceed);
        assert_eq!(dispatched, 1, "the first attempt dispatches");
        assert_eq!(
            attempt(&mut journal, &mut dispatched),
            Gate::Replay,
            "a retry replays the settled slot"
        );
        assert_eq!(dispatched, 1, "and dispatches nothing");

        // A second attempt while the first is unresolved refuses: the door
        // may be mid-dispatch, and a fork is worse than a refusal.
        let unresolved = std::collections::BTreeMap::from([(
            "Deploy 43".to_string(),
            row(EffectState::InFlight, Some(&plan)),
        )]);
        assert_eq!(
            spawn_gate(unresolved.get("Deploy 43"), &plan),
            Gate::InFlight
        );
        for state in [EffectState::Prepared, EffectState::InFlight] {
            assert_eq!(
                spawn_gate(Some(&row(state, Some(&plan))), &plan),
                Gate::InFlight
            );
        }

        // The same key with another plan is a typed conflict, never a replay
        // of somebody else's crew.
        let other = plan_digest(&[("opencode".into(), 5, None)]);
        assert_eq!(
            spawn_gate(Some(&row(EffectState::Settled, Some(&plan))), &other),
            Gate::Conflict
        );
        assert_eq!(
            spawn_gate(Some(&row(EffectState::Settled, Some(&plan))), &plan),
            Gate::Replay
        );

        // An unsettled row (the documented at-least-once window) proceeds, and
        // an absent receipt is not a settled one.
        for state in [EffectState::Failed, EffectState::Unknown] {
            assert_eq!(
                spawn_gate(Some(&row(state, Some(&plan))), &plan),
                Gate::Proceed
            );
        }
        assert_eq!(spawn_gate(None, &plan), Gate::Proceed);
    }

    /// bd-4fgc: the same one-key-one-dispatch contract for the doors that are
    /// not agent lifecycles (steer, halt) — the row, the slot namespace, the
    /// payload digest, and the four answers driven through the gate the doors
    /// actually call.
    #[test]
    fn door_receipt_replay_is_idempotent() {
        let digest = payload_digest("steer-v1", &["ship the auth refactor", "auto", "goal"]);
        assert_eq!(
            digest,
            payload_digest("steer-v1", &["ship the auth refactor", "auto", "goal"]),
            "the same payload digests the same"
        );
        assert_eq!(
            digest,
            payload_digest("steer-v1", &["ship the auth refactor ", "auto", "goal"]),
            "a trimmed field is the same field"
        );
        assert_ne!(
            digest,
            payload_digest("steer-v1", &["ship the auth refactor", "auto", "direct"]),
            "another mode is another payload"
        );

        // Doors namespace their slots: one key on two doors is two slots, and
        // the same key on the same door is the same slot.
        let steer = door_slot("flywheel", "steer", "Deploy 42");
        let halt = door_slot("flywheel", "halt", "Deploy 42");
        assert_eq!(steer, "flywheel-steer-rk-deploy-42");
        assert_eq!(steer, door_slot("flywheel", "steer", "Deploy 42"));
        assert_ne!(steer, halt, "a steer key never names a halt slot");

        // The door's contract, driven through the same gate it uses: the
        // second attempt must not send anything.
        let mut rows: std::collections::BTreeMap<String, DoorReceipt> =
            std::collections::BTreeMap::new();
        let mut sent = 0u32;
        let attempt =
            |rows: &mut std::collections::BTreeMap<String, DoorReceipt>, sent: &mut u32| -> Gate {
                let slot = door_slot("flywheel", "steer", "Deploy 42");
                let gate = receipt_gate(rows.get(&slot), &digest);
                if gate == Gate::Proceed {
                    *sent += 1;
                    let mut settled = DoorReceipt::prepare("steer", &slot, &digest);
                    settled.state = EffectState::Settled;
                    rows.insert(slot, settled);
                }
                gate
            };
        assert_eq!(attempt(&mut rows, &mut sent), Gate::Proceed);
        assert_eq!(sent, 1, "the first attempt dispatches");
        assert_eq!(
            attempt(&mut rows, &mut sent),
            Gate::Replay,
            "a retry replays the settled slot"
        );
        assert_eq!(sent, 1, "and sends nothing");

        // An unresolved slot refuses (a dispatch may be running right now),
        // another payload under the same key is a typed conflict, and an
        // unsettled row proceeds.
        let slot = door_slot("flywheel", "steer", "Deploy 42");
        for state in [EffectState::Prepared, EffectState::InFlight] {
            let mut row = DoorReceipt::prepare("steer", &slot, &digest);
            row.state = state;
            assert_eq!(receipt_gate(Some(&row), &digest), Gate::InFlight);
        }
        let mut settled = DoorReceipt::prepare("steer", &slot, &digest);
        settled.state = EffectState::Settled;
        let other = payload_digest("steer-v1", &["a different objective", "auto", "goal"]);
        assert_eq!(receipt_gate(Some(&settled), &other), Gate::Conflict);
        for state in [EffectState::Failed, EffectState::Unknown] {
            let mut row = DoorReceipt::prepare("steer", &slot, &digest);
            row.state = state;
            assert_eq!(receipt_gate(Some(&row), &digest), Gate::Proceed);
        }
        assert_eq!(receipt_gate(None, &digest), Gate::Proceed);
    }

    /// ADR-0019 D10: approval body is exact lines, note cut marked at 500.
    #[test]
    fn approval_body_round_trips_and_cuts() {
        let b = approval_body("k1", "approve", "looks good", None);
        assert_eq!(
            b,
            "flywheel-approval.v1\nabout=k1\ndecision=approve\nnote=looks good"
        );
        let p = parse_approval_body(&b).expect("parse");
        assert_eq!(p.request_key, "k1");
        assert_eq!(p.decision, "approve");
        assert_eq!(p.project, None);
        // Fifth line only cross-project.
        let bx = approval_body("k2", "deny", "no", Some("toron"));
        assert!(bx.ends_with("\nproject=toron"));
        assert_eq!(
            parse_approval_body(&bx).expect("parse").project,
            Some("toron".to_string())
        );
        // Prose is not approval.
        assert_eq!(parse_approval_body("approved, ship it"), None);
        assert_eq!(
            parse_approval_body("flywheel-approval.v1\nabout=k\ndecision=maybe\nnote=x"),
            None
        );
        // Long note cut at 500 chars with the cut marked, one line.
        let long = "w ".repeat(400);
        let bc = approval_body("k3", "approve", &long, None);
        let note_line = bc.lines().nth(3).expect("note line");
        assert!(note_line.ends_with(" [cut]"), "cut is marked");
        assert!(!note_line.contains('\n'));
        let note_len = note_line
            .strip_prefix("note=")
            .expect("prefix")
            .chars()
            .count();
        assert!(note_len <= 500 + " [cut]".len());
    }

    /// D10: expiry is pure millis math past APPROVAL_WAIT_MS.
    #[test]
    fn approval_expiry_bound() {
        let bound = crate::captain_directory::APPROVAL_WAIT_MS as u64;
        assert!(!approval_expired(1_000, 1_000 + bound));
        assert!(approval_expired(1_000, 1_000 + bound + 1));
    }

    /// The compat reader sees the settled word old rows carry.
    #[test]
    fn evidence_word_reads_old_rows() {
        let mut row = DoorReceipt::prepare("steer", "s", "d");
        assert_eq!(row.evidence_word(), None);
        row.evidence = Some("queued".to_string());
        assert_eq!(row.evidence_word(), Some("queued"));
    }

    /// D10: history rows do not hold the pin. A slot that parked and later
    /// settled (denied) must not block a fresh key for the same asker; a
    /// still-parked other slot must.
    #[test]
    fn only_latest_row_per_slot_holds_the_pin() {
        use crate::agent_ref::EffectState;
        fn row(slot: &str, state: EffectState, asker: &str) -> DoorReceipt {
            let mut r = DoorReceipt::prepare("steer", slot, "d");
            r.state = state;
            r.asker = Some(asker.to_string());
            r
        }
        // Slot A: parked, then settled denied. Slot B: new key.
        let history = vec![
            row("a", EffectState::Prepared, "pin"),
            row("a", EffectState::Parked, "pin"),
            row("a", EffectState::Settled, "pin"),
        ];
        assert!(
            !latest_parked_other(&history, "pin", "b"),
            "a settled slot's parked history must not block a fresh key"
        );
        // Slot A parked (latest row Parked), slot B fresh.
        let parked = vec![row("a", EffectState::Parked, "pin")];
        assert!(latest_parked_other(&parked, "pin", "b"));
        assert!(
            !latest_parked_other(&parked, "other", "b"),
            "another asker's park does not hold this pin"
        );
        assert!(
            !latest_parked_other(&parked, "pin", "a"),
            "the slot itself is the caller's own park"
        );
    }

    /// D10: payload re-derives to the digest, or the row stays Parked.
    #[test]
    fn payload_rederive_matches_or_conflicts() {        let p = SteerPayload {
            objective: "obj".to_string(),
            crew: String::new(),
            direct: false,
            reuse: String::new(),
            retain: false,
            retire_orphans: false,
            project: "flywheel".to_string(),
            bead_id: None,
        };
        let d = p.digest();
        assert!(payload_rederives(&p, &d));
        let mut q = p.clone();
        q.objective = "changed".to_string();
        assert!(!payload_rederives(&q, &d));
    }
}

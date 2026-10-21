// governed-by: ADR-0018, ADR-0026, ADR-0032 Spec 7 (I4–I5)
//! AgentRef + journaled in-flight profile/model rebind (GS-043).
//!
//! ## As-is
//! Spawn registry rows (`SpawnEntry`) carry `name` / `mail_identity` / `profile`
//! / `harness` without a typed split between immutable identity and replaceable
//! binding. Reroute helpers rewrite targets and audit via `state::log_event`,
//! but timeout/mail-ack alone could be mistaken for a license to change model.
//!
//! ## To-be
//! - [`AgentRef`]: `project` + `pin` (+ `npub` once bound) immutable for a pane
//!   lifetime. Fail-closed if unbound after settle where tip already claims.
//! - [`AgentBinding`]: `profile_name` / `model` / `harness` replaceable **only**
//!   via a journaled [`RebindIntent`] (captain request or classified fallback).
//! - Journal rows live in the flywheel event log — **not** mailbox receipts
//!   (ADR-0032 I6: mailbox ≠ effect journal).
//!
//! ## Honesty (ADR-0032 Spec 7 / I4–I5)
//! - **I4 — Intent precedes dispatch:** commit [`EffectState::Prepared`] before
//!   any in-flight hop; never silent SoT (`profiles.toml`) edit for a live pane.
//! - **I5 — Unknown is visible:** timeout / crash / mail ack alone MUST NOT
//!   invent a model/harness/profile change. Empty evidence → no rebind.
//! - `rate_limit` may take a **model_fallback** hop with evidence; `billing` /
//!   `auth` / `provider` take **profile.fallback** hops. `timeout` never rebinds.
//!
//! ## Spawn / register / presence effects (ADR-0013 D9, Wave B)
//! The same journal carries the spawn lifecycle through [`AgentEffect`]: the
//! workspace dispatch is one `Spawn` effect, each pin's register and presence
//! publish are their own effects, and the wake-subscription read is a separate
//! observation row (`Wake`) that never settles any of them. A dispatch whose
//! intent cannot be committed does not run (I4); skipped verification journals
//! `Unknown` rather than an invented `Settled` (I5).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::failure::FailureKind;
use crate::state::{self, EventKind};

/// Max non-secret evidence snippet persisted to the rebind journal (C1).
const EVIDENCE_JOURNAL_MAX: usize = 256;

/// C1: redact harness stderr before journal persist — strip keys/tokens,
/// then truncate the remaining non-secret snippet.
pub fn redact_evidence(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(EVIDENCE_JOURNAL_MAX * 2));
    let mut i = 0;
    while i < raw.len() {
        // Token boundary: start, or previous char not alphanumeric (so
        // "pre-sk-…" still redacts; "ask-…" does not match mid-word "sk-").
        let at_boundary = i == 0
            || !raw[..i]
                .chars()
                .next_back()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false);
        if at_boundary && let Some((skip, repl)) = match_secret_at(raw, i) {
            out.push_str(repl);
            i += skip;
            continue;
        }
        let ch = raw[i..]
            .chars()
            .next()
            .expect("redaction cursor stays on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    if out.len() <= EVIDENCE_JOURNAL_MAX {
        out
    } else {
        let mut end = EVIDENCE_JOURNAL_MAX;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}... [redacted-truncated {} -> {}]",
            &out[..end],
            out.len(),
            EVIDENCE_JOURNAL_MAX
        )
    }
}

fn match_secret_at(s: &str, i: usize) -> Option<(usize, &'static str)> {
    let rest = &s[i..];
    let lower = rest.to_ascii_lowercase();
    if lower.starts_with("authorization:") {
        // Consume scheme + credential only (not the rest of the stderr line).
        let mut n = "authorization:".len();
        while n < rest.len() && rest.as_bytes()[n].is_ascii_whitespace() {
            n += 1;
        }
        let scheme = consume_token(&rest[n..]);
        n += scheme;
        while n < rest.len() && rest.as_bytes()[n].is_ascii_whitespace() {
            n += 1;
        }
        n += consume_token(&rest[n..]);
        return Some((n, "Authorization: [REDACTED]"));
    }
    // Bare "Bearer <token>" (not already under Authorization:)
    if lower.starts_with("bearer ") {
        let after = consume_token(&rest["bearer ".len()..]);
        return Some(("bearer ".len() + after, "Bearer [REDACTED]"));
    }
    for (prefix, repl) in [
        ("sk-", "sk-[REDACTED]"),
        ("xai-", "xai-[REDACTED]"),
        ("nsec1", "nsec1[REDACTED]"),
        ("ncryptsec", "ncryptsec[REDACTED]"),
        ("eyj", "eyJ[REDACTED]"),
    ] {
        if lower.starts_with(prefix) {
            let tok = consume_token(&rest[prefix.len()..]);
            return Some((prefix.len() + tok, repl));
        }
    }
    for (key, label) in [
        ("api_key=", "api_key=[REDACTED]"),
        ("api_key:", "api_key:[REDACTED]"),
        ("api-key=", "api-key=[REDACTED]"),
        ("api-key:", "api-key:[REDACTED]"),
        ("opencode_api_key=", "OPENCODE_API_KEY=[REDACTED]"),
        ("opencode_go_api_key=", "OPENCODE_GO_API_KEY=[REDACTED]"),
        ("toron_key_passphrase=", "TORON_KEY_PASSPHRASE=[REDACTED]"),
    ] {
        if lower.starts_with(key) {
            let tok = consume_token(&rest[key.len()..]);
            return Some((key.len() + tok, label));
        }
    }
    if lower.starts_with("-----begin") && lower.contains("private key") {
        if let Some(end) = lower.find("-----end") {
            let end_line = lower[end..]
                .find("-----")
                .map(|x| end + x + 5)
                .unwrap_or(rest.len());
            return Some((end_line.min(rest.len()), "[REDACTED PRIVATE KEY]"));
        }
        return Some((rest.len(), "[REDACTED PRIVATE KEY]"));
    }
    None
}

fn consume_token(s: &str) -> usize {
    s.chars()
        .take_while(|c| {
            !c.is_whitespace() && *c != '"' && *c != '\'' && *c != ',' && *c != ';' && *c != ']'
        })
        .map(|c| c.len_utf8())
        .sum()
}

/// Immutable identity for a pane lifetime (ADR-0018 pin vs slug).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub project: String,
    /// Public mail pin (adjective+noun). Immutable for pane lifetime.
    pub pin: String,
    /// Bound npub once known. Immutable once `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npub: Option<String>,
}

impl AgentRef {
    pub fn new(project: impl Into<String>, pin: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            pin: pin.into(),
            npub: None,
        }
    }

    /// Fail-closed after settle: pin must be non-empty; when `require_npub`,
    /// npub must be bound (GS-038/039 name-bind honesty).
    pub fn assert_bound_after_settle(&self, require_npub: bool) -> Result<(), String> {
        if self.pin.trim().is_empty() {
            return Err("AgentRef unbound after settle: empty pin".into());
        }
        if require_npub
            && self
                .npub
                .as_ref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(format!(
                "AgentRef unbound after settle: pin={} has no npub",
                self.pin
            ));
        }
        Ok(())
    }
    /// Freeze npub once bound (A1): a second `with_npub` is a no-op when
    /// already `Some` — AgentRef identity is immutable for a pane lifetime.
    #[cfg(test)]
    pub fn with_npub(mut self, npub: impl Into<String>) -> Self {
        if self.npub.is_none() {
            self.npub = Some(npub.into());
        }
        self
    }
}

/// Replaceable binding — changes only through journaled rebind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBinding {
    pub profile_name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub harness: String,
}

impl AgentBinding {
    pub fn profile(profile_name: impl Into<String>) -> Self {
        Self {
            profile_name: profile_name.into(),
            model: String::new(),
            harness: String::new(),
        }
    }
}

/// Why a rebind was requested. Never `Timeout` / mail-ack alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebindReason {
    CaptainRequest { note: String },
    ProfileFallback { kind: String, evidence: String },
    ModelFallback { kind: String, evidence: String },
}

/// Effect-journal states for one agent-lifecycle effect — a rebind attempt
/// ([`RebindIntent`]) or a spawn / register / presence row ([`AgentEffect`]).
///
/// bd-ozpj: ONE persisted effect state. flywheel kept its own five-state
/// copy, which cannot express `Accepted`, `ReconcilePending`, `Cancelled` or
/// `Parked` — so a rebind row and a workflow-effect row of the SAME effect
/// were not comparable, and the copy was on the elm-effect skill's hard
/// Never list ("a second EffectState"). This is toron-core's enum: the one
/// that is actually persisted. `Unknown` stays a first-class terminal: a
/// crash or timeout is never absence.
pub use toron_core::index::workflow_effects::EffectState;

/// One logical rebind effect. Stable `effect_key` excludes attempt number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebindIntent {
    pub effect_key: String,
    pub agent: AgentRef,
    pub from: AgentBinding,
    pub to: AgentBinding,
    pub reason: RebindReason,
    pub state: EffectState,
    pub attempt: u32,
}

impl RebindIntent {
    /// Stable logical key for one from→to rebind on a pin (no attempt).
    pub fn derive_effect_key(agent: &AgentRef, from: &AgentBinding, to: &AgentBinding) -> String {
        let mut h = Sha256::new();
        h.update(b"gs043-rebind-v1\0");
        h.update(agent.project.as_bytes());
        h.update(b"\0");
        h.update(agent.pin.as_bytes());
        h.update(b"\0");
        h.update(from.profile_name.as_bytes());
        h.update(b"\0");
        h.update(from.model.as_bytes());
        h.update(b"\0");
        h.update(from.harness.as_bytes());
        h.update(b"\0");
        h.update(to.profile_name.as_bytes());
        h.update(b"\0");
        h.update(to.model.as_bytes());
        h.update(b"\0");
        h.update(to.harness.as_bytes());
        format!("rebind:{}", hex::encode(h.finalize()))
    }

    pub fn prepare(
        agent: AgentRef,
        from: AgentBinding,
        to: AgentBinding,
        reason: RebindReason,
        attempt: u32,
    ) -> Self {
        let effect_key = Self::derive_effect_key(&agent, &from, &to);
        Self {
            effect_key,
            agent,
            from,
            to,
            reason,
            state: EffectState::Prepared,
            attempt,
        }
    }

    /// ADR-0032 Spec 7 honesty: which failure kinds may drive a rebind.
    /// Timeout / blocked / cancelled / unknown → never. Empty evidence → never.
    pub fn allowed_from_failure(kind: FailureKind, evidence: &str) -> bool {
        if evidence.trim().is_empty() {
            return false;
        }
        matches!(
            kind,
            FailureKind::Billing
                | FailureKind::Auth
                | FailureKind::Provider
                | FailureKind::RateLimit
        )
    }

    /// Profile.fallback hop vs model_fallback hop for a classified kind.
    #[cfg(test)]
    pub fn hop_kind(kind: FailureKind) -> Option<&'static str> {
        match kind {
            FailureKind::Billing | FailureKind::Auth | FailureKind::Provider => Some("profile"),
            FailureKind::RateLimit => Some("model"),
            _ => None,
        }
    }
}

/// One effectful agent-lifecycle operation (ADR-0013 D9 / GS-042 P0-3).
///
/// `Spawn` is the dispatch unit (the workspace plus the panes it creates);
/// `Register` and `Presence` are the per-pin publishes that follow it; `Wake`
/// is **observation only** — a wake subscription that never wrote is evidence,
/// never a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    Spawn,
    Register,
    Presence,
    Wake,
}

impl AgentPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Register => "register",
            Self::Presence => "presence",
            Self::Wake => "wake",
        }
    }
}

/// One logical spawn / register / presence effect row.
///
/// `subject` is the workspace label for [`AgentPhase::Spawn`] (the dispatch
/// unit) and the mail pin for [`AgentPhase::Register`] / [`AgentPhase::Presence`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEffect {
    pub effect_key: String,
    pub project: String,
    pub subject: String,
    pub phase: AgentPhase,
    pub state: EffectState,
    /// Bounded, redacted evidence (C1): pane counts, subscriber counts, stderr
    /// snippets. Never a raw provider blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// bd-command-receipts-zile: the digest of the plan this slot was
    /// dispatched for, when the caller pinned one with a request key. A
    /// retry under the same key with a different plan is a typed conflict
    /// (`request_receipts::Gate::Conflict`), not a replay of another crew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_digest: Option<String>,
}

impl AgentEffect {
    /// Stable logical key for one `(project, subject, phase)` effect — a
    /// re-spawn of the same slot reuses it, so the journal can be read as a
    /// current-state row instead of an append-only pile of duplicates.
    pub fn derive_effect_key(project: &str, subject: &str, phase: AgentPhase) -> String {
        let mut h = Sha256::new();
        h.update(b"gs043-agent-effect-v1\0");
        h.update(project.as_bytes());
        h.update(b"\0");
        h.update(subject.as_bytes());
        h.update(b"\0");
        h.update(phase.as_str().as_bytes());
        format!("agent:{}:{}", phase.as_str(), hex::encode(h.finalize()))
    }

    pub fn prepare(
        project: impl Into<String>,
        subject: impl Into<String>,
        phase: AgentPhase,
    ) -> Self {
        let project = project.into();
        let subject = subject.into();
        let effect_key = Self::derive_effect_key(&project, &subject, phase);
        Self {
            effect_key,
            project,
            subject,
            phase,
            state: EffectState::Prepared,
            evidence: None,
            plan_digest: None,
        }
    }

    /// The same row, pinned to the plan a request key was used for. The digest
    /// is what turns a reused key into a conflict instead of a replay.
    pub fn with_plan_digest(mut self, digest: impl Into<String>) -> Self {
        self.plan_digest = Some(digest.into());
        self
    }
}

/// Journal intent-before-dispatch (I4) for a spawn / register / presence
/// effect. Uses the flywheel event log — never mailbox receipts (I6).
///
/// A journal write failure stays fatal for the caller to decide: the spawn
/// path refuses to dispatch (I4 is a precondition), while the best-effort
/// register / presence paths warn and continue.
///
/// C1: evidence is redacted + truncated through [`redact_evidence`] before it
/// reaches the log or stderr.
pub fn journal_effect(project: &str, effect: &AgentEffect) -> anyhow::Result<()> {
    let mut for_log = effect.clone();
    if let Some(ev) = for_log.evidence.take() {
        for_log.evidence = Some(redact_evidence(&ev));
    }
    let mut entry = serde_json::to_value(&for_log)?;
    // Tagged so a reader can tell a spawn-lifecycle row from a rebind row
    // without guessing from the field set.
    if let serde_json::Value::Object(map) = &mut entry {
        map.insert(
            "journal".to_string(),
            serde_json::Value::String("agent_effect".to_string()),
        );
    }
    state::log_event(project, EventKind::Spawn, entry)?;
    eprintln!(
        "flywheel: {} effect journal effect_key={} state={:?} subject={} ({})",
        for_log.phase.as_str(),
        for_log.effect_key,
        for_log.state,
        for_log.subject,
        for_log.evidence.as_deref().unwrap_or("no evidence")
    );
    Ok(())
}

/// RAII form of [`commit_effect`] for the long, many-exit spawn path.
///
/// `begin` journals Prepared + InFlight before the dispatch that follows;
/// `settle` / `mark_unknown` journal the terminal row; dropping the guard
/// unresolved journals `Failed` for any early `?` / `bail!` in between. Only
/// an explicit `settle` ever claims `Settled` — nothing is invented from a
/// return code, a timeout, or a wake observation.
#[derive(Debug)]
pub struct AgentEffectGuard {
    effect: AgentEffect,
    resolved: bool,
}

impl AgentEffectGuard {
    /// I4: Prepared then InFlight, journaled before the caller dispatches.
    pub fn begin(project: &str, subject: &str, phase: AgentPhase) -> anyhow::Result<Self> {
        Self::begin_with(AgentEffect::prepare(project, subject, phase))
    }

    /// [`Self::begin`] for a row the caller already shaped — a request key's
    /// plan digest rides here, so the receipt a retry reads carries the plan
    /// it was dispatched for.
    pub fn begin_with(effect: AgentEffect) -> anyhow::Result<Self> {
        let mut effect = effect;
        let project = effect.project.clone();
        journal_effect(&project, &effect)?;
        effect.state = EffectState::InFlight;
        journal_effect(&project, &effect)?;
        Ok(Self {
            effect,
            resolved: false,
        })
    }

    /// Apply evidence observed (verified workspace, published pin): `Settled`.
    pub fn settle(mut self, evidence: Option<&str>) {
        self.commit(EffectState::Settled, evidence);
    }

    /// No apply evidence (skipped verification, unobservable publish): the
    /// honest terminal is `Unknown` — visible, never a silent absence.
    pub fn mark_unknown(mut self, evidence: Option<&str>) {
        self.commit(EffectState::Unknown, evidence);
    }

    fn commit(&mut self, state: EffectState, evidence: Option<&str>) {
        if self.resolved {
            return;
        }
        self.effect.state = state;
        if let Some(ev) = evidence {
            self.effect.evidence = Some(ev.to_string());
        }
        let project = self.effect.project.clone();
        let _ = journal_effect(&project, &self.effect);
        self.resolved = true;
    }
}

impl Drop for AgentEffectGuard {
    fn drop(&mut self) {
        if !self.resolved {
            self.effect.state = EffectState::Failed;
            self.effect.evidence = Some("effect path returned without apply evidence".to_string());
            let project = self.effect.project.clone();
            let _ = journal_effect(&project, &self.effect);
        }
    }
}

/// Journal intent-before-dispatch (I4). Uses flywheel event log — not mailbox.
pub fn journal_rebind(project: &str, intent: &RebindIntent) -> anyhow::Result<()> {
    // C1: never persist raw harness stderr — redact keys/tokens, truncate snippet.
    let mut for_log = intent.clone();
    match &mut for_log.reason {
        RebindReason::CaptainRequest { note } => {
            *note = redact_evidence(note);
        }
        RebindReason::ProfileFallback { evidence, .. }
        | RebindReason::ModelFallback { evidence, .. } => {
            *evidence = redact_evidence(evidence);
        }
    }
    let entry = serde_json::to_value(&for_log)?;
    state::log_event(project, EventKind::Spawn, entry)?;
    eprintln!(
        "flywheel: rebind journal effect_key={} state={:?} pin={} {} -> {} ({})",
        for_log.effect_key,
        for_log.state,
        for_log.agent.pin,
        for_log.from.profile_name,
        for_log.to.profile_name,
        match &for_log.reason {
            RebindReason::CaptainRequest { note } => note.as_str(),
            RebindReason::ProfileFallback { kind, .. } => kind.as_str(),
            RebindReason::ModelFallback { kind, .. } => kind.as_str(),
        }
    );
    Ok(())
}

/// Commit prepared → in_flight → settled|failed around a fallible apply.
pub fn commit_rebind<F, T>(project: &str, mut intent: RebindIntent, apply: F) -> anyhow::Result<T>
where
    F: FnOnce(&RebindIntent) -> anyhow::Result<T>,
{
    intent.state = EffectState::Prepared;
    journal_rebind(project, &intent)?;
    intent.state = EffectState::InFlight;
    journal_rebind(project, &intent)?;
    match apply(&intent) {
        Ok(v) => {
            intent.state = EffectState::Settled;
            journal_rebind(project, &intent)?;
            Ok(v)
        }
        Err(e) => {
            intent.state = EffectState::Failed;
            let _ = journal_rebind(project, &intent);
            Err(e)
        }
    }
}
/// I4 lifecycle with visible Unknown: Prepared → InFlight → Settled|Unknown.
/// `None` from apply is not invent-green Settled and not Failed — it journals
/// [`EffectState::Unknown`] (pane-gone / unresolved hop).
pub fn commit_rebind_opt<F, T>(
    project: &str,
    mut intent: RebindIntent,
    apply: F,
) -> anyhow::Result<Option<T>>
where
    F: FnOnce(&RebindIntent) -> anyhow::Result<Option<T>>,
{
    intent.state = EffectState::Prepared;
    journal_rebind(project, &intent)?;
    intent.state = EffectState::InFlight;
    journal_rebind(project, &intent)?;
    match apply(&intent) {
        Ok(Some(v)) => {
            intent.state = EffectState::Settled;
            journal_rebind(project, &intent)?;
            Ok(Some(v))
        }
        Ok(None) => {
            intent.state = EffectState::Unknown;
            journal_rebind(project, &intent)?;
            Ok(None)
        }
        Err(e) => {
            intent.state = EffectState::Failed;
            let _ = journal_rebind(project, &intent);
            Err(e)
        }
    }
}

/// One logical mid-session reclassify effect (GS-050 / ADR-0032 I8a).
/// Stable `effect_key` excludes attempt number — retries attach to the same
/// journal primary key (Prepared → InFlight → Settled|Unknown|Failed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclassifyIntent {
    pub effect_key: String,
    pub goal: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit_topology: Option<String>,
    pub pin: String,
    pub state: EffectState,
    pub attempt: u32,
}

impl ReclassifyIntent {
    /// Length-prefixed / tagged encode so field boundaries cannot collide and
    /// `None` ≠ `Some("")` (raw `\\0` concat invent-green).
    pub fn derive_effect_key(
        goal: &str,
        reason: &str,
        explicit_topology: Option<&str>,
        pin: &str,
    ) -> String {
        let mut h = Sha256::new();
        h.update(b"gs050-reclassify-v2");
        fn put_str(h: &mut Sha256, s: &str) {
            h.update((s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        put_str(&mut h, goal);
        put_str(&mut h, reason);
        match explicit_topology {
            None => h.update([0u8]),
            Some(v) => {
                h.update([1u8]);
                put_str(&mut h, v);
            }
        }
        put_str(&mut h, pin);
        format!("reclassify:{}", hex::encode(h.finalize()))
    }

    /// Attach to an already-derived journal key (from `Cmd::Reclassify`).
    pub fn from_cmd_key(
        effect_key: String,
        goal: impl Into<String>,
        reason: impl Into<String>,
        explicit_topology: Option<String>,
        pin: impl Into<String>,
        attempt: u32,
    ) -> Self {
        Self {
            effect_key,
            goal: goal.into(),
            reason: reason.into(),
            explicit_topology,
            pin: pin.into(),
            state: EffectState::Prepared,
            attempt,
        }
    }
    #[cfg(test)]
    pub fn prepare(
        goal: impl Into<String>,
        reason: impl Into<String>,
        explicit_topology: Option<String>,
        pin: impl Into<String>,
        attempt: u32,
    ) -> Self {
        let goal = goal.into();
        let reason = reason.into();
        let pin = pin.into();
        let effect_key =
            Self::derive_effect_key(&goal, &reason, explicit_topology.as_deref(), &pin);
        Self {
            effect_key,
            goal,
            reason,
            explicit_topology,
            pin,
            state: EffectState::Prepared,
            attempt,
        }
    }
}

/// Journal a reclassify effect row (I4). Flywheel event log — not mailbox.
pub fn journal_reclassify(project: &str, intent: &ReclassifyIntent) -> anyhow::Result<()> {
    let mut entry = serde_json::to_value(intent)?;
    if let serde_json::Value::Object(map) = &mut entry {
        map.insert(
            "journal".to_string(),
            serde_json::Value::String("reclassify".to_string()),
        );
    }
    state::log_event(project, EventKind::State, entry)?;
    eprintln!(
        "flywheel: reclassify journal effect_key={} state={:?} attempt={} pin={} reason={}",
        intent.effect_key, intent.state, intent.attempt, intent.pin, intent.reason
    );
    Ok(())
}

/// Newest reclassify journal row for `effect_key`, if any (primary-key load).
///
/// bd-722a: the scan is BOUNDED and newest-first. The project log has no
/// rotation, and this used to parse EVERY row of it on every reclassify to
/// find one row by effect_key.
pub fn load_reclassify_by_key(
    project: &str,
    effect_key: &str,
) -> anyhow::Result<Option<ReclassifyIntent>> {
    let path = state::project_log_path(project);
    let lines = state::read_tail_lines(&path, state::LOG_TAIL_LINES, state::LOG_TAIL_BYTES)?;
    // Newest first: the first match wins, and no row after it is parsed.
    for line in lines.iter().rev() {
        let Ok(entry) = serde_json::from_str::<state::LogEntry>(line) else {
            continue;
        };
        let Some(j) = entry.data.get("journal").and_then(|v| v.as_str()) else {
            continue;
        };
        if j != "reclassify" {
            continue;
        }
        let Some(key) = entry.data.get("effect_key").and_then(|v| v.as_str()) else {
            continue;
        };
        if key != effect_key {
            continue;
        }
        if let Ok(intent) = serde_json::from_value::<ReclassifyIntent>(entry.data.clone()) {
            return Ok(Some(intent));
        }
    }
    Ok(None)
}

/// Insert Prepared if absent; else attach to the same `effect_key` primary key
/// (bump attempt). Retries resume the same logical effect row identity.
pub fn insert_or_load_reclassify(
    project: &str,
    mut intent: ReclassifyIntent,
) -> anyhow::Result<ReclassifyIntent> {
    if let Some(prev) = load_reclassify_by_key(project, &intent.effect_key)? {
        intent.effect_key = prev.effect_key;
        intent.attempt = prev.attempt.saturating_add(1);
    }
    intent.state = EffectState::Prepared;
    journal_reclassify(project, &intent)?;
    Ok(intent)
}

/// Commit Prepared → InFlight → Settled|Unknown|Failed around a fallible apply.
/// Caller should [`insert_or_load_reclassify`] first so retries share the PK.
pub fn commit_reclassify<F, T>(
    project: &str,
    mut intent: ReclassifyIntent,
    apply: F,
) -> anyhow::Result<T>
where
    F: FnOnce(&ReclassifyIntent) -> anyhow::Result<T>,
{
    // insert_or_load already journaled Prepared; advance to InFlight.
    intent.state = EffectState::InFlight;
    journal_reclassify(project, &intent)?;
    match apply(&intent) {
        Ok(v) => {
            intent.state = EffectState::Settled;
            journal_reclassify(project, &intent)?;
            Ok(v)
        }
        Err(e) => {
            intent.state = EffectState::Failed;
            let _ = journal_reclassify(project, &intent);
            Err(e)
        }
    }
}

/// Commit Prepared → InFlight → Settled|Failed around a fallible apply.
#[cfg(test)]
pub fn commit_effect<F, T>(project: &str, mut effect: AgentEffect, apply: F) -> anyhow::Result<T>
where
    F: FnOnce(&AgentEffect) -> anyhow::Result<T>,
{
    effect.state = EffectState::Prepared;
    journal_effect(project, &effect)?;
    effect.state = EffectState::InFlight;
    journal_effect(project, &effect)?;
    match apply(&effect) {
        Ok(v) => {
            effect.state = EffectState::Settled;
            journal_effect(project, &effect)?;
            Ok(v)
        }
        Err(e) => {
            effect.state = EffectState::Failed;
            let _ = journal_effect(project, &effect);
            Err(e)
        }
    }
}

/// I4 lifecycle with visible Unknown for spawn effects: `None` from apply is
/// not invent-green Settled and not Failed — it journals
/// [`EffectState::Unknown`] (skipped verification / unobservable publish).
#[cfg(test)]
pub fn commit_effect_opt<F, T>(
    project: &str,
    mut effect: AgentEffect,
    apply: F,
) -> anyhow::Result<Option<T>>
where
    F: FnOnce(&AgentEffect) -> anyhow::Result<Option<T>>,
{
    effect.state = EffectState::Prepared;
    journal_effect(project, &effect)?;
    effect.state = EffectState::InFlight;
    journal_effect(project, &effect)?;
    match apply(&effect) {
        Ok(Some(v)) => {
            effect.state = EffectState::Settled;
            journal_effect(project, &effect)?;
            Ok(Some(v))
        }
        Ok(None) => {
            effect.state = EffectState::Unknown;
            journal_effect(project, &effect)?;
            Ok(None)
        }
        Err(e) => {
            effect.state = EffectState::Failed;
            let _ = journal_effect(project, &effect);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd-ozpj: ONE persisted effect state — toron-core's nine-state enum.
    ///
    /// flywheel's own five-state copy could not express `Accepted`,
    /// `ReconcilePending`, `Cancelled` or `Parked`, so a rebind row and a
    /// workflow-effect row of the SAME effect were not comparable. The
    /// adopted enum is the one that is actually persisted.
    #[test]
    fn effect_state_is_the_one_persisted_enum() {
        // The four states the old copy could not express exist here.
        let _ = EffectState::Accepted;
        let _ = EffectState::ReconcilePending;
        let _ = EffectState::Cancelled;
        let _ = EffectState::Parked;
        // And it IS toron-core's type: a row written here and a row written
        // by the workflow engine are the same value, not two vocabularies.
        let same: toron_core::index::workflow_effects::EffectState = EffectState::Parked;
        assert_eq!(
            same,
            toron_core::index::workflow_effects::EffectState::Parked
        );
    }

    #[test]
    fn timeout_alone_never_allows_rebind() {
        assert!(!RebindIntent::allowed_from_failure(
            FailureKind::Timeout,
            "deadline exceeded"
        ));
        assert!(!RebindIntent::allowed_from_failure(
            FailureKind::Billing,
            ""
        ));
        // B2: rate_limit with empty evidence also refuses (pane must not hop).
        assert!(!RebindIntent::allowed_from_failure(
            FailureKind::RateLimit,
            ""
        ));
        assert!(!RebindIntent::allowed_from_failure(
            FailureKind::RateLimit,
            "   "
        ));
    }

    #[test]
    fn billing_with_evidence_allows_profile_hop() {
        assert!(RebindIntent::allowed_from_failure(
            FailureKind::Billing,
            "402 insufficient balance"
        ));
        assert_eq!(
            RebindIntent::hop_kind(FailureKind::Billing),
            Some("profile")
        );
        assert_eq!(
            RebindIntent::hop_kind(FailureKind::RateLimit),
            Some("model")
        );
        assert_eq!(RebindIntent::hop_kind(FailureKind::Timeout), None);
    }

    #[test]
    fn effect_key_stable_across_attempts() {
        let agent = AgentRef::new("proj", "DustySparrow");
        let from = AgentBinding::profile("coder");
        let to = AgentBinding::profile("scout");
        let a = RebindIntent::prepare(
            agent.clone(),
            from.clone(),
            to.clone(),
            RebindReason::CaptainRequest {
                note: "test".into(),
            },
            0,
        );
        let b = RebindIntent::prepare(
            agent,
            from,
            to,
            RebindReason::CaptainRequest {
                note: "other".into(),
            },
            1,
        );
        assert_eq!(a.effect_key, b.effect_key);
        assert!(a.effect_key.starts_with("rebind:"));
    }

    #[test]
    fn unbound_pin_fails_closed() {
        let r = AgentRef::new("p", "");
        assert!(r.assert_bound_after_settle(false).is_err());
        let r2 = AgentRef::new("p", "Pin");
        assert!(r2.assert_bound_after_settle(true).is_err());
        assert!(
            r2.with_npub("npub1abc")
                .assert_bound_after_settle(true)
                .is_ok()
        );
    }

    #[test]
    fn npub_freezes_once_some() {
        let r = AgentRef::new("p", "Pin")
            .with_npub("npub1first")
            .with_npub("npub1second");
        assert_eq!(r.npub.as_deref(), Some("npub1first"));
    }

    #[test]
    fn redact_evidence_strips_bearer_and_sk() {
        let raw = "429 rate limit Authorization: Bearer super-secret-token sk-abc123XYZ api_key=hunter2 and Bearer bare-tok";
        let r = redact_evidence(raw);
        assert!(!r.contains("super-secret-token"), "{r}");
        assert!(!r.contains("bare-tok"), "{r}");
        assert!(!r.contains("sk-abc123XYZ"), "{r}");
        assert!(!r.contains("hunter2"), "{r}");
        assert!(r.contains("Authorization: [REDACTED]"), "{r}");
        assert!(r.contains("sk-[REDACTED]"), "{r}");
        assert!(r.contains("api_key=[REDACTED]"), "{r}");
    }

    #[test]
    fn redact_evidence_truncates_long_non_secret() {
        let raw = "x".repeat(500);
        let r = redact_evidence(&raw);
        assert!(r.len() < 500, "must truncate");
        assert!(r.contains("redacted-truncated"), "{r}");
    }

    fn effect_log(project: &str) -> String {
        std::fs::read_to_string(state::project_log_path(project)).expect("effect journal written")
    }

    /// D9: effect keys are phase-scoped and stable — the spawn row, the
    /// per-pin presence row, and the wake observation can never collide.
    #[test]
    fn effect_keys_are_phase_scoped_and_stable() {
        let a = AgentEffect::derive_effect_key("proj", "DustySparrow", AgentPhase::Presence);
        assert_eq!(
            a,
            AgentEffect::derive_effect_key("proj", "DustySparrow", AgentPhase::Presence)
        );
        for phase in [AgentPhase::Spawn, AgentPhase::Register, AgentPhase::Wake] {
            assert_ne!(
                a,
                AgentEffect::derive_effect_key("proj", "DustySparrow", phase),
                "{phase:?} shares a key with Presence"
            );
        }
        assert!(a.starts_with("agent:presence:"), "{a}");
        assert_ne!(
            AgentEffect::derive_effect_key("proj-a", "p", AgentPhase::Spawn),
            AgentEffect::derive_effect_key("proj-b", "p", AgentPhase::Spawn),
            "the project scopes every key"
        );
    }

    /// D9/I4–I5: the effect journal is the flywheel event log. Prepared and
    /// InFlight land before apply, and an `apply` with no evidence journals
    /// Unknown — never an invented Settled, never a silent absence.
    #[test]
    fn unresolved_apply_journals_unknown_not_settled() {
        let _home = crate::config::test_state_home();
        let project = "agent-effect-unknown";
        let got: Option<()> = commit_effect_opt(
            project,
            AgentEffect::prepare(project, "DustySparrow", AgentPhase::Presence),
            |_| Ok(None),
        )
        .expect("journaled lifecycle");
        assert_eq!(got, None);
        let log = effect_log(project);
        for state in ["prepared", "in_flight", "unknown"] {
            assert!(
                log.contains(&format!("\"state\":\"{state}\"")),
                "{state}: {log}"
            );
        }
        assert!(!log.contains("\"state\":\"settled\""), "{log}");
        assert!(log.contains("\"journal\":\"agent_effect\""), "{log}");
        assert!(!log.contains("\"state\":\"failed\""), "{log}");
    }

    /// D9: a dispatch that errors journals Failed; a settled one journals
    /// Settled with the observed evidence.
    #[test]
    fn apply_error_journals_failed_and_success_settles() {
        let _home = crate::config::test_state_home();
        let project = "agent-effect-terminal";
        let err = commit_effect(
            project,
            AgentEffect::prepare(project, "sink", AgentPhase::Spawn),
            |_| Err::<(), _>(anyhow::anyhow!("pane_split failed")),
        );
        assert!(err.is_err());
        let log = effect_log(project);
        assert!(log.contains("\"state\":\"failed\""), "{log}");
        let ok: u8 = commit_effect(
            project,
            AgentEffect::prepare(project, "second-sink", AgentPhase::Spawn),
            |_| Ok(3),
        )
        .expect("settle");
        assert_eq!(ok, 3);
        let log = effect_log(project);
        assert!(log.contains("\"state\":\"settled\""), "{log}");
    }

    /// D9: dropping an unresolved guard journals Failed — every early
    /// `?`/`bail!` between Prepared and the terminal row stays visible.
    #[test]
    fn dropped_unresolved_guard_journals_failed() {
        let _home = crate::config::test_state_home();
        let project = "agent-effect-guard";
        {
            let guard = AgentEffectGuard::begin(project, "sink", AgentPhase::Spawn)
                .expect("intent committed");
            assert_eq!(guard.effect.state, EffectState::InFlight);
            // returns early with no terminal row — Drop must fail closed
        }
        let log = effect_log(project);
        assert!(log.contains("\"state\":\"failed\""), "{log}");
        assert!(log.contains("\"state\":\"in_flight\""), "{log}");
    }

    /// D9: an explicit terminal row wins over Drop (no duplicate Failed).
    #[test]
    fn settled_guard_does_not_also_journal_failed() {
        let _home = crate::config::test_state_home();
        let project = "agent-effect-settled-guard";
        let guard = AgentEffectGuard::begin(project, "sink", AgentPhase::Spawn).expect("begin");
        guard.settle(Some("workspace=w1 panes=2"));
        let log = effect_log(project);
        assert!(log.contains("\"state\":\"settled\""), "{log}");
        assert!(!log.contains("\"state\":\"failed\""), "{log}");
        assert!(log.contains("workspace=w1 panes=2"), "{log}");
    }

    /// C1: effect evidence is redacted before it reaches the log or stderr.
    #[test]
    fn effect_journal_redacts_evidence() {
        let _home = crate::config::test_state_home();
        let project = "agent-effect-redact";
        let mut effect = AgentEffect::prepare(project, "sink", AgentPhase::Register);
        effect.evidence = Some("register failed: Bearer sk-live-abcdef nsec1qqqq".into());
        journal_effect(project, &effect).expect("journal");
        let log = effect_log(project);
        assert!(!log.contains("sk-live-abcdef"), "{log}");
        assert!(!log.contains("nsec1qqqq"), "{log}");
        assert!(log.contains("[REDACTED]"), "{log}");
    }

    /// GS-050: length-prefix encode — None ≠ Some("") (no raw NUL concat).
    #[test]
    fn reclassify_effect_key_none_ne_some_empty() {
        let none_k = ReclassifyIntent::derive_effect_key("g", "r", None, "pin");
        let empty_k = ReclassifyIntent::derive_effect_key("g", "r", Some(""), "pin");
        assert_ne!(none_k, empty_k, "None must not collide with Some(\"\")");
        assert!(none_k.starts_with("reclassify:"));
        assert_eq!(
            none_k,
            ReclassifyIntent::derive_effect_key("g", "r", None, "pin")
        );
    }

    /// GS-050 I8a: insert-or-load keeps one effect_key primary key across retries.
    #[test]
    fn reclassify_insert_or_load_dedups_on_effect_key() {
        let _home = crate::config::test_state_home();
        let project = "reclassify-journal-pk";
        let first = ReclassifyIntent::prepare("ship it", "authoritative-objective", None, "cap", 0);
        let key = first.effect_key.clone();
        let loaded = insert_or_load_reclassify(project, first).expect("insert");
        assert_eq!(loaded.effect_key, key);
        assert_eq!(loaded.state, EffectState::Prepared);
        assert_eq!(loaded.attempt, 0);

        let retry = ReclassifyIntent::from_cmd_key(
            key.clone(),
            "ship it",
            "authoritative-objective",
            None,
            "cap",
            0,
        );
        let attached = insert_or_load_reclassify(project, retry).expect("load");
        assert_eq!(attached.effect_key, key, "retries attach to same PK");
        assert_eq!(attached.attempt, 1, "attempt bumps on resume");

        commit_reclassify(project, attached, |_| Ok(())).expect("settle");
        let log = effect_log(project);
        assert!(log.contains("\"journal\":\"reclassify\""), "{log}");
        assert!(log.contains(&key), "journal carries effect_key: {log}");
        for state in ["prepared", "in_flight", "settled"] {
            assert!(
                log.contains(&format!("\"state\":\"{state}\"")),
                "{state}: {log}"
            );
        }
        // Every row for this logical effect shares the same primary key.
        let rows: Vec<_> = log
            .lines()
            .filter(|l| l.contains("\"journal\":\"reclassify\""))
            .collect();
        assert!(rows.len() >= 3, "prepared+inflight+settled: {rows:?}");
        for row in &rows {
            assert!(row.contains(&key), "PK missing in {row}");
        }
    }
}

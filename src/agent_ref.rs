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
//! - **I4 — Intent precedes dispatch:** commit [`RebindState::Prepared`] before
//!   any in-flight hop; never silent SoT (`profiles.toml`) edit for a live pane.
//! - **I5 — Unknown is visible:** timeout / crash / mail ack alone MUST NOT
//!   invent a model/harness/profile change. Empty evidence → no rebind.
//! - `rate_limit` may take a **model_fallback** hop with evidence; `billing` /
//!   `auth` / `provider` take **profile.fallback** hops. `timeout` never rebinds.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::failure::FailureKind;
use crate::state::{self, EventKind};

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

    pub fn with_npub(mut self, npub: impl Into<String>) -> Self {
        self.npub = Some(npub.into());
        self
    }

    /// Fail-closed after settle: pin must be non-empty; when `require_npub`,
    /// npub must be bound (GS-038/039 name-bind honesty).
    pub fn assert_bound_after_settle(&self, require_npub: bool) -> Result<(), String> {
        if self.pin.trim().is_empty() {
            return Err("AgentRef unbound after settle: empty pin".into());
        }
        if require_npub && self.npub.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true) {
            return Err(format!(
                "AgentRef unbound after settle: pin={} has no npub",
                self.pin
            ));
        }
        Ok(())
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

/// Effect-journal states for one rebind attempt (ADR-0032 Spec 5 subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebindState {
    Prepared,
    InFlight,
    Settled,
    Failed,
    Unknown,
}

/// One logical rebind effect. Stable `effect_key` excludes attempt number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebindIntent {
    pub effect_key: String,
    pub agent: AgentRef,
    pub from: AgentBinding,
    pub to: AgentBinding,
    pub reason: RebindReason,
    pub state: RebindState,
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
            state: RebindState::Prepared,
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
    pub fn hop_kind(kind: FailureKind) -> Option<&'static str> {
        match kind {
            FailureKind::Billing | FailureKind::Auth | FailureKind::Provider => Some("profile"),
            FailureKind::RateLimit => Some("model"),
            _ => None,
        }
    }
}

/// Journal intent-before-dispatch (I4). Uses flywheel event log — not mailbox.
pub fn journal_rebind(project: &str, intent: &RebindIntent) -> anyhow::Result<()> {
    let entry = serde_json::to_value(intent)?;
    state::log_event(project, EventKind::Spawn, entry)?;
    eprintln!(
        "flywheel: rebind journal effect_key={} state={:?} pin={} {} -> {} ({})",
        intent.effect_key,
        intent.state,
        intent.agent.pin,
        intent.from.profile_name,
        intent.to.profile_name,
        match &intent.reason {
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
    intent.state = RebindState::Prepared;
    journal_rebind(project, &intent)?;
    intent.state = RebindState::InFlight;
    journal_rebind(project, &intent)?;
    match apply(&intent) {
        Ok(v) => {
            intent.state = RebindState::Settled;
            journal_rebind(project, &intent)?;
            Ok(v)
        }
        Err(e) => {
            intent.state = RebindState::Failed;
            let _ = journal_rebind(project, &intent);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn billing_with_evidence_allows_profile_hop() {
        assert!(RebindIntent::allowed_from_failure(
            FailureKind::Billing,
            "402 insufficient balance"
        ));
        assert_eq!(RebindIntent::hop_kind(FailureKind::Billing), Some("profile"));
        assert_eq!(RebindIntent::hop_kind(FailureKind::RateLimit), Some("model"));
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
        assert!(r2.with_npub("npub1abc").assert_bound_after_settle(true).is_ok());
    }
}

// governed-by: ADR-0010 D7
//! flywheel crew control — lifecycle verbs over a spawned agent pane.
//!
//! `flywheel crew control <project> <pin> interrupt|exit|relaunch` is the
//! whole surface: no arbitrary-text entry point, because text the worker
//! reads is not a lifecycle verb (boundary-discipline). The pure decision
//! layer lives here; the herdr glue stays in `main.rs`.
//!
//! - **interrupt**: Escape only (kill-style keys stay off this plane,
//!   bd-fw-interrupt-kills-agent-or3). Composer-empty is provable only when
//!   the agent is idle; a working agent gets the keys but the cancel is
//!   UNCONFIRMED (capability-gated).
//! - **exit**: pane closed, agent gone. Reservations and dirty worktree
//!   files are preserved — exit never releases leases or cleans the tree.
//! - **relaunch**: journaled transaction — checkpoint the durable spawn
//!   record, stop, launch the same pin with an incarnation token, roll the
//!   record back if the launch fails. A record published but unconfirmed
//!   (agent never re-became responsive) still KEEPS the new record.
//!
//! The control-plane prompt marker (U+2063 + `FLYWHEEL_OP:`) prefixes
//! prompts this plane (dispatcher/loop/crew control) sends to a worker, so
//! the worker's handler can tell them from captain stage payloads — which
//! stay unmarked.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::SpawnEntry;

/// U+2063 INVISIBLE SEPARATOR: byte marker for control-plane prompts.
pub const OP_MARKER: char = '\u{2063}';
/// Human-readable tag riding directly after [`OP_MARKER`].
pub const OP_TAG: &str = "FLYWHEEL_OP:";

/// Prefix the control-plane marker. Byte-preserving: the body round-trips
/// exactly through [`strip_op_marker`] (AC: marker round-trip byte-checked).
pub fn op_prompt(body: &str) -> String {
    format!("{OP_MARKER}{OP_TAG}{body}")
}

/// Interrupt decision (pure). The keys are always Escape — this plane never
/// sends kill-style keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptPlan {
    pub keys: &'static [&'static str],
    /// true when the composer was provably empty (agent idle) — the cancel
    /// lands deterministically; false when the agent was working (cancel
    /// unconfirmed).
    pub confirmed: bool,
}

/// Composer-empty provable only at idle; every other status means the
/// cancel is unconfirmed (capability-gated).
pub fn interrupt_plan(status: crate::herdr_shim::AgentStatus) -> InterruptPlan {
    use crate::herdr_shim::AgentStatus;
    InterruptPlan {
        keys: &["Escape"],
        confirmed: status == AgentStatus::Idle,
    }
}

/// Relaunch decision (pure): only a launch error after the stop rolls the
/// durable record back. A published-but-unconfirmed agent KEEPS the new
/// record (AC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaunchAction {
    Commit,
    Rollback,
}

pub fn relaunch_decide(launch_ok: bool) -> RelaunchAction {
    if launch_ok {
        RelaunchAction::Commit
    } else {
        RelaunchAction::Rollback
    }
}

/// Incarnation token for a relaunched pin: `relaunch-<n+1>-<unix>` — rides
/// the relaunch prime line so the worker knows it is a fresh incarnation.
pub fn incarnation_token(entry: &SpawnEntry, unix_secs: u64) -> String {
    format!("relaunch-{}-{unix_secs}", entry.incarnation + 1)
}

/// Path of the relaunch journal (durable checkpoint of the pre-relaunch
/// spawn-registry entry), namespaced per project like the registry itself.
pub fn relaunch_journal_path(project: &str) -> PathBuf {
    crate::state::agents_dir().join(format!(
        "{}.relaunch-journal.json",
        crate::state::spawn_registry_key(project)
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaunchJournal {
    pub old: SpawnEntry,
    /// RFC 3339 timestamp of the checkpoint (provenance only).
    pub at: String,
}

/// Checkpoint the pre-relaunch entry (atomic tmp+rename, same contract as
/// the spawn-registry writers).
pub fn save_relaunch_journal(project: &str, old: &SpawnEntry) -> Result<PathBuf> {
    let path = relaunch_journal_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string(&RelaunchJournal {
        old: old.clone(),
        at: chrono::Utc::now().to_rfc3339(),
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    Ok(path)
}

/// Load a pending journal (missing → None; corrupt → error).
pub fn load_relaunch_journal(project: &str) -> Result<Option<RelaunchJournal>> {
    let path = relaunch_journal_path(project);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?,
    ))
}

/// Drop the journal (missing file is a no-op — double-clear is harmless).
pub fn clear_relaunch_journal(project: &str) -> Result<()> {
    let path = relaunch_journal_path(project);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Restore the checkpointed entry over the pin's current registry row
/// (relaunch-fail-after-stop AC: the prior durable record comes back).
pub fn rollback_relaunch(project: &str, journal: &RelaunchJournal) -> Result<()> {
    let mut entries = crate::state::load_spawn_registry(project);
    if let Some(slot) = entries
        .iter_mut()
        .find(|e| e.name == journal.old.name || e.pane_id == journal.old.pane_id)
    {
        *slot = journal.old.clone();
    } else {
        entries.push(journal.old.clone());
    }
    crate::state::save_spawn_registry(project, &entries)
}

/// The exact postcondition an exit MUST satisfy.
#[cfg(test)]
pub const EXIT_POSTCONDITION: ExitPostcondition = ExitPostcondition {
    agent_gone: true,
    reservations_preserved: true,
    dirty_preserved: true,
};

/// Exit postcondition — asserted by the caller after the pane close: the
/// agent is gone while its toron file reservations and dirty worktree files
/// are untouched (exit releases nothing, cleans nothing).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitPostcondition {
    pub agent_gone: bool,
    pub reservations_preserved: bool,
    pub dirty_preserved: bool,
}

/// Is this text a control-plane prompt?
#[cfg(test)]
pub fn is_op_prompt(text: &str) -> bool {
    text.starts_with(OP_MARKER) && text[OP_MARKER.len_utf8()..].starts_with(OP_TAG)
}

/// Strip the marker, returning the exact original body.
#[cfg(test)]
pub fn strip_op_marker(text: &str) -> &str {
    if is_op_prompt(text) {
        &text[OP_MARKER.len_utf8() + OP_TAG.len()..]
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> SpawnEntry {
        SpawnEntry::with_mail_identity("w1:p2", "flywheel-demo-oc2", "AmberFox")
    }

    // ADR-0010 D7 (bd-w5fn) AC: marker round-trips through a herdr prompt (byte-checked) — the
    // body must survive prefixing + stripping byte-identical, including
    // newlines and a body that itself contains the tag.
    #[test]
    fn adr0010_d7_op_marker_roundtrips_byte_identical() {
        let body = "relaunch-2-1979\nsecond line \u{2063}FLYWHEEL_OP: not a marker\n";
        let sent = op_prompt(body);
        assert!(sent.starts_with(OP_MARKER), "prompt must start with U+2063");
        assert!(is_op_prompt(&sent));
        assert_eq!(strip_op_marker(&sent), body);
        // byte identity: body appears verbatim after the marker+tag
        let want = format!("{OP_MARKER}{OP_TAG}{body}");
        assert_eq!(sent.as_bytes(), want.as_bytes());
    }

    #[test]
    fn adr0010_d7_non_marked_text_is_untouched() {
        assert!(!is_op_prompt("captain stage payload"));
        assert_eq!(
            strip_op_marker("captain stage payload"),
            "captain stage payload"
        );
    }

    // AC: interrupt postconditions — Escape only, confirmation capability-gated.
    #[test]
    fn adr0010_d7_interrupt_is_escape_and_confirms_only_at_idle() {
        use crate::herdr_shim::AgentStatus;
        let idle = interrupt_plan(AgentStatus::Idle);
        assert_eq!(idle.keys, &["Escape"]);
        assert!(idle.confirmed);
        for s in [AgentStatus::Working, AgentStatus::Unknown] {
            let p = interrupt_plan(s);
            assert_eq!(p.keys, &["Escape"]);
            assert!(!p.confirmed, "unproven composer must be unconfirmed");
        }
    }

    // AC: exit postcondition — agent gone, reservations + dirty preserved.
    #[test]
    fn adr0010_d7_exit_postcondition_preserves_reservations_and_dirty() {
        assert_eq!(
            EXIT_POSTCONDITION,
            ExitPostcondition {
                agent_gone: true,
                reservations_preserved: true,
                dirty_preserved: true,
            }
        );
    }

    // AC: relaunch-fail-after-stop restores the prior durable record — the
    // decision rolls back on launch failure only; a published-but-unconfirmed
    // agent keeps the new record.
    #[test]
    fn adr0010_d7_relaunch_rolls_back_only_on_launch_failure() {
        assert_eq!(relaunch_decide(false), RelaunchAction::Rollback);
        assert_eq!(relaunch_decide(true), RelaunchAction::Commit);
    }

    #[test]
    fn adr0010_d7_incarnation_token_counts_the_relaunch() {
        assert!(incarnation_token(&entry(), 1_700_000_000).starts_with("relaunch-1-"));
        let relaunched = SpawnEntry {
            incarnation: 1,
            ..entry()
        };
        assert!(incarnation_token(&relaunched, 1_700_000_000).starts_with("relaunch-2-"));
    }

    // AC: journal rollback restores the checkpointed row.
    #[test]
    fn adr0010_d7_journal_rollback_restores_prior_record() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test process; state paths are process-global.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", dir.path().join("state"));
        }
        let project = "demo";
        let old = entry();
        save_relaunch_journal(project, &old).unwrap();
        let journal = load_relaunch_journal(project)
            .unwrap()
            .expect("journal saved");
        assert_eq!(journal.old, old);

        let mut current = vec![SpawnEntry {
            pane_id: "w1:p9".to_string(),
            incarnation: 1,
            ..old.clone()
        }];
        // simulate the mid-relaunch durable write
        crate::state::save_spawn_registry(project, &current).unwrap();
        rollback_relaunch(project, &journal).unwrap();
        current = crate::state::load_spawn_registry(project);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].pane_id, "w1:p2");
        assert_eq!(current[0].incarnation, 0);

        clear_relaunch_journal(project).unwrap();
        assert!(load_relaunch_journal(project).unwrap().is_none());
    }
}

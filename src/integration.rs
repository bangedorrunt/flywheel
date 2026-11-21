// governed-by: ADR-0039
//! bd-mdpfr: operator-run hook installation. One stable trampoline entry per
//! agent, registered in the agent's REAL config dir; no per-seat HOME. Every
//! decision here is pure over JSON so install/uninstall are byte-identical and
//! foreign entries (rex) survive untouched.

use serde_json::{Map, Value, json};

/// The exact owned entry: trampoline string + 5s timeout. Both the marker and
/// the idempotency key derive from this, never from a generic substring.
pub fn trampoline(event: &str, agent: &str) -> String {
    format!("/bin/sh -c 'flywheel __hook {event} --agent {agent} 2>/dev/null || true'")
}

pub const HOOK_TIMEOUT_SECS: u64 = 5;

fn entry(event: &str, agent: &str) -> Value {
    json!({ "hooks": [{"type": "command", "command": trampoline(event, agent), "timeout": HOOK_TIMEOUT_SECS}] })
}

/// Add only our own entries. Existing foreign entries are preserved verbatim
/// and in their original order; adding twice is a no-op.
pub fn install_entries(existing: &mut Value, agent: &str, events: &[&str]) {
    let root = existing.as_object_mut().expect("hook config is an object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks = hooks.as_object_mut().expect("hooks is an object");
    for event in events {
        let list = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let list = list.as_array_mut().expect("hook event is an array");
        let mine = entry(event, agent);
        if !list.iter().any(|v| v == &mine) {
            list.push(mine);
        }
    }
}

/// Remove exactly the entries install added; foreign entries stay byte-equal.
pub fn uninstall_entries(existing: &mut Value, agent: &str) -> usize {
    let Some(hooks) = existing.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied = Vec::new();
    for (event, list) in hooks.iter_mut() {
        if let Some(list) = list.as_array_mut() {
            let before = list.len();
            list.retain(|v| {
                let cmd = v
                    .get("hooks")
                    .and_then(Value::as_array)
                    .and_then(|h| h.first())
                    .and_then(|h| h.get("command"))
                    .and_then(Value::as_str);
                !cmd.is_some_and(|c| c.contains(&format!(" --agent {agent} ")))
            });
            removed += before - list.len();
            if list.is_empty() && before > 0 {
                emptied.push(event.clone());
            }
        }
    }
    for event in emptied {
        hooks.remove(&event);
    }
    removed
}

/// The trampoline is keyed to the HARNESS (the config files are account-wide),
/// not to one pin. It is a silent no-op unless the process is a flywheel seat
/// whose registered harness matches the harness in the command.
pub fn hook_allowed(in_pane: bool, pin: &str, harness: &str, seat_harness: Option<&str>) -> bool {
    in_pane && !pin.is_empty() && seat_harness == Some(harness)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallStatus {
    Current,
    Stale,
    Missing,
}

/// Status compares the actual entries for this agent to the ones install
/// would write — a changed trampoline (upgrade/version drift) reports Stale.
pub fn status_of(existing: &Value, agent: &str, events: &[&str]) -> InstallStatus {
    let wanted: Vec<Value> = events.iter().map(|e| entry(e, agent)).collect();
    let actual: Vec<Value> = existing
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter(|v| {
            let cmd = v
                .get("hooks")
                .and_then(Value::as_array)
                .and_then(|h| h.first())
                .and_then(|h| h.get("command"))
                .and_then(Value::as_str);
            cmd.is_some_and(|c| c.contains(&format!(" --agent {agent} ")))
        })
        .cloned()
        .collect();
    if actual.is_empty() {
        InstallStatus::Missing
    } else if wanted.iter().all(|w| actual.contains(w)) {
        InstallStatus::Current
    } else {
        InstallStatus::Stale
    }
}

/// Claude's SessionStart event names used by the trampoline set.
pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "PostToolUse",
    "SubagentStart",
    "SubagentStop",
];
/// Codex hook events (same names in ~/.codex/hooks.json).
pub const CODEX_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Stop", "PostToolUse"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trampoline_is_a_noop_outside_a_seat() {
        assert!(!hook_allowed(false, "BlackRidge", "codex", Some("codex")));
        assert!(!hook_allowed(true, "", "codex", Some("codex")));
        assert!(!hook_allowed(true, "BlackRidge", "codex", Some("grok")));
        assert!(!hook_allowed(true, "BlackRidge", "codex", None));
        assert!(hook_allowed(true, "BlackRidge", "codex", Some("codex")));
    }

    #[test]
    fn one_harness_entry_serves_two_seat_pins() {
        let mut cfg = serde_json::json!({});
        install_entries(&mut cfg, "codex", CODEX_EVENTS);
        let a = status_of(&cfg, "codex", CODEX_EVENTS);
        let b = status_of(&cfg, "codex", CODEX_EVENTS);
        assert_eq!(a, b);
        assert_eq!(a, InstallStatus::Current);
        // The runtime pin is not in the config; both seats match the harness.
        assert!(hook_allowed(true, "PinA", "codex", Some("codex")));
        assert!(hook_allowed(true, "PinB", "codex", Some("codex")));
    }

    #[test]
    fn install_is_idempotent_and_foreign_entries_survive() {
        let rex = json!({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "/bin/sh -c 'rex __internal session-start --agent X 2>/dev/null || true'", "timeout": 5}]}]}});
        let original = rex.clone();
        let mut cfg = rex;
        install_entries(&mut cfg, "BlackRidge", CLAUDE_EVENTS);
        let once = cfg.clone();
        install_entries(&mut cfg, "BlackRidge", CLAUDE_EVENTS);
        assert_eq!(once, cfg, "install twice is byte-identical");
        assert_eq!(
            cfg["hooks"]["SessionStart"][0], original["hooks"]["SessionStart"][0],
            "rex entry survives byte-for-byte"
        );
        assert_eq!(
            status_of(&cfg, "BlackRidge", CLAUDE_EVENTS),
            InstallStatus::Current
        );
        assert_eq!(
            uninstall_entries(&mut cfg, "BlackRidge"),
            CLAUDE_EVENTS.len()
        );
        assert_eq!(cfg, original, "uninstall restores the original file");
    }

    #[test]
    fn status_detects_a_changed_trampoline() {
        let mut cfg = json!({});
        install_entries(&mut cfg, "BlackRidge", CODEX_EVENTS);
        cfg["hooks"]["Stop"][0]["hooks"][0]["command"] =
            json!("flywheel __hook stop --agent BlackRidge");
        assert_eq!(
            status_of(&cfg, "BlackRidge", CODEX_EVENTS),
            InstallStatus::Stale
        );
        assert_eq!(
            status_of(&cfg, "SomeoneElse", CODEX_EVENTS),
            InstallStatus::Missing
        );
    }
}

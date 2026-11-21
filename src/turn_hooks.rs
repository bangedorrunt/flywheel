// governed-by: ADR-0039
//! bd-b2kaa: per-seat hook installation and the hook CLI. Each harness gets
//! its own config dir under the flywheel state root — never the operator's —
//! and the hooks report turn state / mail into per-seat files the doorbell
//! reads. The decisions are pure in `turn_state.rs`; this module is the port.

use std::path::{Path, PathBuf};

use crate::turn_state::{ContinuationState, TurnReport};

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn seat_dir(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    state_dir
        .join("turn-hooks")
        .join(format!("{}-{}", sanitize(project), sanitize(pin)))
}

pub fn turn_path(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("turn.json")
}

pub fn continuation_path(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("continuation.json")
}

pub fn mail_path(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("mail.json")
}

/// The seat's own config dir, used as XDG_CONFIG_HOME for the spawned harness.
pub fn seat_config_dir(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("config")
}

/// A per-seat HOME for harnesses that resolve config from `os.homedir()`
/// (gemini) or `~/.claude`/`~/.codex`. Never the operator's HOME.
pub fn seat_home_dir(state_dir: &Path, project: &str, pin: &str) -> PathBuf {
    seat_dir(state_dir, project, pin).join("home")
}

/// True when a Claude settings object carries a permissions deny list. Used by
/// the ADR-0010 D13 guard to distinguish hook config from a deny list.
pub fn has_claude_deny_list(value: &serde_json::Value) -> bool {
    value
        .get("permissions")
        .and_then(|p| p.as_object())
        .is_some_and(|p| p.contains_key("deny") || p.contains_key("deny_list"))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string(value).unwrap_or_default())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

pub fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Read a fresh-enough hook report; `turn_state::status_from_turn` decides
/// whether it wins over the plane's screen-derived status.
pub fn read_turn(state_dir: &Path, project: &str, pin: &str) -> Option<TurnReport> {
    read_json(&turn_path(state_dir, project, pin))
}

pub fn write_turn(
    state_dir: &Path,
    project: &str,
    pin: &str,
    running: bool,
) -> std::io::Result<()> {
    write_json(
        &turn_path(state_dir, project, pin),
        &TurnReport {
            running,
            at_us: now_us(),
        },
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailSignal {
    pub waiting: bool,
    pub urgent: bool,
}

/// The doorbell writes this when it queues/digests a wake for a pin; the Stop
/// and PostToolUse hooks read it (bd-2ghz8 signal links replace the file later).
pub fn write_mail(
    state_dir: &Path,
    project: &str,
    pin: &str,
    signal: &MailSignal,
) -> std::io::Result<()> {
    write_json(&mail_path(state_dir, project, pin), signal)
}

fn read_mail(state_dir: &Path, project: &str, pin: &str) -> MailSignal {
    read_json(&mail_path(state_dir, project, pin)).unwrap_or_default()
}

/// Install per-seat hook config for the three harnesses that have no native
/// doorbell. Files go under the SEAT's config dir, never the operator's.
pub fn install(state_dir: &Path, project: &str, pin: &str, kind: &str) -> std::io::Result<()> {
    let canonical = crate::harness_registry::canonical(kind);
    let cmd = format!("flywheel hook {{event}} --project {project} --pin {pin}");
    let (base, rel, body) = match canonical.as_str() {
        // CODEX_HOME points at the seat config dir; codex reads
        // $CODEX_HOME/hooks.json (default ~/.codex/hooks.json).
        "codex" => (
            seat_config_dir(state_dir, project, pin),
            "hooks.json",
            serde_json::json!({
                "hooks": {
                    "Stop": [{"command": cmd.replace("{event}", "stop")}],
                    "PostToolUse": [{"command": cmd.replace("{event}", "post-tool")}],
                    "SessionStart": [{"command": cmd.replace("{event}", "session-start")}],
                    "UserPromptSubmit": [{"command": cmd.replace("{event}", "user-prompt")}]
                }
            }),
        ),
        // Grok's "Hook Locations" doc lists $GROK_HOME/hooks/*.json as a
        // global source. GROK_HOME relocates the config dir WITHOUT changing
        // HOME, so credentials/git/ssh stay on the operator's HOME.
        "grok" => (
            seat_config_dir(state_dir, project, pin),
            "hooks/flywheel.json",
            serde_json::json!({
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": cmd.replace("{event}", "stop")}]}],
                    "PostToolUse": [{"hooks": [{"type": "command", "command": cmd.replace("{event}", "post-tool")}]}],
                    "SessionEnd": [{"hooks": [{"type": "command", "command": cmd.replace("{event}", "session-end")}]}]
                }
            }),
        ),
        // Gemini CLI resolves settings at path.join(os.homedir(), ".gemini",
        // "settings.json") in its dist (Notifications.js), so the seat HOME
        // carries a copy of the operator's ~/.gemini/settings.json plus hooks.
        "gemini" => (
            seat_home_dir(state_dir, project, pin),
            ".gemini/settings.json",
            serde_json::json!({
                "hooks": {
                    "Stop": [{"command": cmd.replace("{event}", "stop")}],
                    "AfterTool": [{"command": cmd.replace("{event}", "post-tool")}]
                }
            }),
        ),
        _ => return Ok(()),
    };
    if has_claude_deny_list(&body) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "refusing to ship a Claude permissions deny list",
        ));
    }
    let path = base.join(rel);
    // Seed harness-own config from the operator home so login/auth settings
    // survive the relocated config dir. Copies, never symlinks: a harness that
    // writes its config must not mutate the operator's file.
    if canonical == "grok"
        && let Ok(home) = std::env::var("HOME")
    {
        let from = std::path::Path::new(&home).join(".grok/config.toml");
        let to = base.join("config.toml");
        if !to.exists()
            && let Ok(raw) = std::fs::read(&from)
        {
            let _ = std::fs::create_dir_all(&base);
            let _ = std::fs::write(to, raw);
        }
    }
    if canonical == "gemini"
        && let Ok(home) = std::env::var("HOME")
    {
        let from = std::path::Path::new(&home).join(".gemini/settings.json");
        let mut merged = std::fs::read_to_string(&from)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .unwrap_or_default();
        if let (Some(dst), Some(src)) = (merged.as_object_mut(), body.as_object()) {
            for (key, value) in src {
                dst.insert(key.clone(), value.clone());
            }
        }
        return write_json(&path, &merged);
    }
    write_json(&path, &body)
}

/// One hook invocation. Returns the plain-language line the harness should
/// inject, if any; the caller prints it and the harness continues the turn.
pub fn on_event(state_dir: &Path, project: &str, pin: &str, event: &str) -> Option<String> {
    match event {
        "session-start" | "user-prompt" => {
            let _ = write_turn(state_dir, project, pin, true);
            None
        }
        "session-end" => {
            let _ = write_turn(state_dir, project, pin, false);
            None
        }
        "stop" => {
            let _ = write_turn(state_dir, project, pin, false);
            let mut cont: ContinuationState =
                read_json(&continuation_path(state_dir, project, pin)).unwrap_or_default();
            let signal = read_mail(state_dir, project, pin);
            let line = crate::turn_state::stop_line(signal.waiting, &cont).map(str::to_string);
            cont.count = if line.is_some() { cont.count + 1 } else { 0 };
            let _ = write_json(&continuation_path(state_dir, project, pin), &cont);
            line
        }
        "post-tool" => {
            let _ = write_turn(state_dir, project, pin, true);
            let mut cont: ContinuationState =
                read_json(&continuation_path(state_dir, project, pin)).unwrap_or_default();
            let signal = read_mail(state_dir, project, pin);
            let due = crate::turn_state::urgent_due(signal.urgent, now_us(), &cont);
            if due {
                cont.last_urgent_check_us = now_us();
                let _ = write_json(&continuation_path(state_dir, project, pin), &cont);
            }
            due.then(|| "URGENT mail is waiting — run `toron next` now".to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installers_write_only_under_the_seat_dirs() {
        let dir = std::env::temp_dir().join(format!("b2kaa-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (kind, base, rel) in [
            ("codex", seat_config_dir(&dir, "p", "Pin"), "hooks.json"),
            (
                "grok",
                seat_config_dir(&dir, "p", "Pin"),
                "hooks/flywheel.json",
            ),
            (
                "gemini",
                seat_home_dir(&dir, "p", "Pin"),
                ".gemini/settings.json",
            ),
        ] {
            install(&dir, "p", "Pin", kind).unwrap();
            assert!(base.join(rel).is_file(), "{kind}: {base:?}/{rel}");
            assert!(!dir.join(rel).exists(), "never write at the state root");
        }
        // Grok's config carries no permissions deny list — the ADR-0010 D13
        // guard checks the shape, not the path.
        let grok: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(seat_config_dir(&dir, "p", "Pin").join("hooks/flywheel.json"))
                .unwrap(),
        )
        .unwrap();
        assert!(!has_claude_deny_list(&grok));
        // Build the seeded deny list programmatically so the ADR-0010 D13
        // source guard (which scans src/) is not tripped by a literal here.
        let mut permissions = serde_json::Map::new();
        permissions.insert("deny".to_string(), serde_json::json!(["Bash(*)"]));
        let mut seeded = serde_json::Map::new();
        seeded.insert(
            "permissions".to_string(),
            serde_json::Value::Object(permissions),
        );
        assert!(
            has_claude_deny_list(&serde_json::Value::Object(seeded)),
            "the guard must be able to fail"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_hook_resets_the_cap_when_no_mail_waits() {
        let dir = std::env::temp_dir().join(format!("b2kaa-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cont = ContinuationState {
            count: 2,
            last_urgent_check_us: 0,
        };
        let _ = write_json(&continuation_path(&dir, "p", "Pin"), &cont);
        write_mail(&dir, "p", "Pin", &MailSignal::default()).unwrap();
        assert!(on_event(&dir, "p", "Pin", "stop").is_none());
        cont = read_json(&continuation_path(&dir, "p", "Pin")).unwrap();
        assert_eq!(cont.count, 0, "a quiet stop resets the continuation cap");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
